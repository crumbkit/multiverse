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
//! child alone, each under its private reference. When the app wants an
//! owner for a volume the process mounts, it creates the container with
//! `docker compose up --no-start` first, because Docker copies an image
//! directory's owner onto an empty volume at that moment, then gives each
//! such volume's root directory the app's owner through the volume-owner
//! step of [`super::volume_owner`], and only then starts the container, so
//! the app's main process never runs with the wrong owner. A volume that
//! only processes `up` leaves running mount gets the same check, once, and
//! a check that cannot run becomes a note and leaves the container running.
//! The pull of the app's image and of the helper's image and the
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
//!   it took before the container exited. Only Docker's `running` status
//!   counts. When no such read comes before the process's ready timeout,
//!   which includes a Docker whose every read fails, the process fails as
//!   not ready, or the start is uncertain when the caller's deadline
//!   comes first.
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
//! container's result, and `stop` goes on to the next. It then removes the
//! app's leftover volume-owner helpers.
//!
//! Every removal of a container, in `stop`, in the replacement of a
//! container by `start` and in the clean-up after a failed start, removes
//! the container's anonymous volumes with it. docker makes one for each
//! `VOLUME` path of the image, and nothing records it, so it would
//! otherwise stay behind. The workspace's named volumes stay.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::calendar::{civil_from_days, days_from_civil};
use crate::cancel::Cancel;
use crate::clock::{POLL_INTERVAL, now, pause};
use crate::engine::container::ContainerError;
use crate::engine::container::app::{self, reference};
use crate::engine::container::command::{
    DOCKER_DEADLINE, DockerExec, MOST_OUTPUT_BYTES, Referenced, check, check_compose, message,
    quick, secret_exec, write_secret,
};
use crate::engine::container::compose::{AppStep, AppUp, Up, app_up};
use crate::engine::container::names::{self, Identity, Scope, VolumeKey};
use crate::engine::container::objects::{
    self, ContainerState, Inspected, ensure_image, ensure_volume, inspect_id, inspect_named,
    no_restart, reinspect, remove, stop, stop_until,
};
use crate::engine::container::secrets::{self, ImageConfig, SecretBytes};
use crate::engine::container::volume_owner;
use crate::engine::containers::{
    Before, ContainerObjects, ContainerRewrite, ContainerRunner, ContainerStart, ContainerStop,
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
use crate::record::run::{ContainerEntry, PlanHash, VolumeOwner};
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
    /// The image that `start` makes sure Docker has before it runs a
    /// volume-owner helper, which only a test changes.
    helper_image: String,
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
    pub(crate) fn new(state_dir: PathBuf, start_grace: Duration) -> Self {
        Self {
            state_dir,
            start_grace,
            docker_environment: None,
            helper_image: volume_owner::HELPER_IMAGE.to_owned(),
        }
    }

    /// Returns this runner with `image` as the helper image it pulls.
    #[cfg(test)]
    pub(crate) fn with_helper_image(mut self, image: String) -> Self {
        self.helper_image = image;
        self
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

    /// Returns the plan hash of the container of `process` of `app` in
    /// `scope` with the values of `environment`: [`app::plan_hash`] with
    /// the keyed digest of the values of its variables that are not
    /// secrets, under the key in phase's state directory.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::StateUnwritable`] when phase cannot create or
    /// use the key file, [`PhaseError::ProcessUnstartable`] when the
    /// environment cannot write a value, and the errors of
    /// [`app::plan_hash`].
    fn planned_hash(
        &self,
        scope: &Scope,
        app: &AppPlan,
        process: &ProcessPlan,
        environment: &dyn AppliedEnvironment,
    ) -> Result<PlanHash, PhaseError> {
        let key = crate::env::digest::load_key(&self.state_dir)?;
        let values = app::values_digest(app, process, environment, &key).map_err(|reason| {
            PhaseError::ProcessUnstartable {
                app: app.name.clone(),
                process: process.name.clone(),
                reason,
            }
        })?;
        app::plan_hash(scope, app, &process.name, values.as_ref())
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

/// Returns the failure of a `docker compose up` of `service` that Docker
/// failed because a port is allocated, when the port is not one that the
/// process publishes, with Docker's own error line `docker_line`.
fn unpublished_port(service: &names::ServiceName, docker_line: Option<String>) -> Halt {
    Halt::Failed(docker_error(
        &ContainerError::unexpected(
            "compose up",
            Some(service.as_str()),
            "docker reported a port that the process does not publish as allocated",
        )
        .with_docker_line(docker_line),
    ))
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

/// Returns `checked` as the start goes on from it. A failure to check the
/// owner of a running process's volume, because Docker refused a step, a
/// helper failed or the app's image cannot be read, leaves the container
/// running, so it becomes a note in `started`. A cancelled start and a
/// failure to write the record still end the start.
fn unchecked_owners(
    checked: Result<(), Halt>,
    started: &mut ContainersStarted,
) -> Result<(), Halt> {
    match checked {
        Err(Halt::Failed(error @ (PhaseError::Docker { .. } | PhaseError::VolumeOwner { .. }))) => {
            started.owners_unchecked = Some(error.to_string());
            Ok(())
        }
        other => other,
    }
}

/// Returns the volumes, with their record names, that the processes `up`
/// leaves running mount and that no process it starts mounts.
fn running_volumes(
    start: &ContainerStart<'_>,
    scope: &Scope,
) -> Result<Vec<(VolumeKey, DockerName)>, Halt> {
    let starting = app::process_volume_keys(start.app, start.processes);
    app::process_volume_keys(start.app, start.running)
        .into_iter()
        .filter(|key| !starting.contains(key))
        .map(|key| volume_docker_name(scope, &key).map(|name| (key, name)))
        .collect()
}

/// Returns the name of the volume `key` of `scope` as the run record
/// holds it.
fn volume_docker_name(scope: &Scope, key: &VolumeKey) -> Result<DockerName, Halt> {
    scope.volume_name(key).as_str().parse().map_err(|_| {
        Halt::Failed(docker_error(&ContainerError::unexpected(
            "volume create",
            None,
            format!("{} is not a Docker name", scope.volume_name(key)),
        )))
    })
}

/// Gives the root directory of the volume `key` to `owner` through a
/// helper. The caller has made sure Docker has the helper's image. The
/// record names no owner for the volume while the helper runs, so a run
/// that ends meanwhile claims none, and names `owner` once it has run.
fn give_volume(
    start: &ContainerStart<'_>,
    scope: &Scope,
    key: &VolumeKey,
    owner: &VolumeOwner,
    before: &mut dyn FnMut(Before<'_>) -> Result<(), PhaseError>,
    started: &mut ContainersStarted,
) -> Result<(), Halt> {
    let name = volume_docker_name(scope, key)?;
    let recorded = |started: &mut ContainersStarted, owner: Option<VolumeOwner>| {
        if let Some((_, entry)) = started
            .volumes
            .iter_mut()
            .find(|(volume, _)| *volume == name)
        {
            *entry = owner;
        }
    };
    recorded(started, None);
    before(Before::VolumeOwner(&name))?;
    volume_owner::set_owner(scope, key, owner, start.deadline, start.cancel)
        .map_err(|error| halt(&error))?;
    recorded(started, Some(owner.clone()));
    Ok(())
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

/// One process of a start on its way from no container to a started one.
struct Launch<'a> {
    /// The start it belongs to.
    start: &'a ContainerStart<'a>,
    /// The scope of its app.
    scope: &'a Scope,
    /// The empty project directory of the compose project.
    dir: &'a std::path::Path,
    /// The compose file for this process's environment.
    file: &'a serde_json::Value,
    /// The process's service.
    service: &'a names::ServiceName,
    /// The process's name.
    name: &'a ProcessName,
    /// The process.
    process: &'a ProcessPlan,
    /// The hash of the process's service in the plan.
    plan_hash: PlanHash,
    /// Whether the process receives secret files.
    secret_files: bool,
}

impl Launch<'_> {
    /// Runs `docker compose up` with `step` for the process, with the
    /// values of `env`, for at most `remaining`.
    fn compose(
        &self,
        env: Referenced<'_>,
        step: AppStep,
        remaining: Duration,
    ) -> Result<Up, ContainerError> {
        let app = self.start.app;
        let runs_as_image_of = match &app.source {
            Source::Image { image } if app.container.user.is_none() => Some(image.as_str()),
            _ => None,
        };
        app_up(
            &AppUp {
                scope: self.scope,
                dir: self.dir,
                file: self.file,
                service: self.service,
                step,
                runs_as_image_of,
            },
            env,
            (remaining, self.start.cancel),
        )
    }

    /// Reports the container `inspected` of the process in `started`.
    fn report(
        &self,
        inspected: &Inspected,
        started: &mut ContainersStarted,
    ) -> Result<(), PhaseError> {
        started.containers.push(StartedContainer {
            process: self.name.clone(),
            id: record_id(inspected)?,
            plan_hash: self.plan_hash.clone(),
            ready: None,
        });
        Ok(())
    }

    /// Returns whether the start goes on after the `docker compose up`
    /// that ended as `up`. A taken port and a deadline end the start with
    /// their verdict recorded in `started`, and any other failure ends it
    /// as an error.
    fn settled(
        &self,
        up: Result<Up, ContainerError>,
        started: &mut ContainersStarted,
    ) -> Result<bool, Halt> {
        match up {
            Ok(Up::Started) => Ok(true),
            Ok(Up::PortAllocated {
                port: number,
                docker_line,
            }) => {
                let Some(port) = port_named(self.start.app, self.process, number) else {
                    return Err(unpublished_port(self.service, docker_line));
                };
                started.problem = Some((
                    self.name.clone(),
                    StartProblem::port_taken(port, Vec::new()),
                ));
                Ok(false)
            }
            Err(error @ ContainerError::Cancelled { .. }) => Err(halt(&error)),
            // The ready deadline stopped `compose up`, so phase does not
            // know whether the container would have started.
            Err(_) if now() >= self.start.deadline => {
                started.uncertain = Some(self.name.clone());
                Ok(false)
            }
            Err(error) => Err(halt(&no_shell_hint(error, self.secret_files))),
        }
    }

    /// Creates the process's container with the values of `env` and gives
    /// each of `owned`'s volumes to its owner, and returns whether the
    /// start goes on. Docker copies an image directory's owner onto an
    /// empty volume when it creates the container, so the owner is set
    /// after the creation and before the start. The created container is
    /// reported, so a failure that follows removes it.
    fn create_and_own(
        &self,
        env: Referenced<'_>,
        remaining: Duration,
        owned: (&VolumeOwner, &BTreeSet<VolumeKey>),
        before: &mut dyn FnMut(Before<'_>) -> Result<(), PhaseError>,
        started: &mut ContainersStarted,
    ) -> Result<bool, Halt> {
        let created = self.compose(env, AppStep::Create, remaining);
        let inspected = inspect_named(self.scope, self.service).map_err(|error| halt(&error))?;
        if let Some(inspected) = &inspected {
            self.report(inspected, started)?;
        }
        if !self.settled(created, started)? {
            return Ok(false);
        }
        if inspected.is_none() {
            started.problem = Some((self.name.clone(), StartProblem::exited(None, None)));
            return Ok(false);
        }
        let (owner, volumes) = owned;
        for key in volumes {
            give_volume(self.start, self.scope, key, owner, before, started)?;
        }
        Ok(true)
    }
}

impl ComposeRunner {
    /// Starts the processes of `start` into `started`, as the module docs
    /// say, and returns what ended the start early.
    fn start_into(
        &self,
        start: &ContainerStart<'_>,
        before: &mut dyn FnMut(Before<'_>) -> Result<(), PhaseError>,
        started: &mut ContainersStarted,
    ) -> Result<(), Halt> {
        let app = start.app;
        let scope = self.scope(start.workspace, &app.name);
        if start.processes.is_empty() {
            let checked = check().map_err(|error| halt(&error)).and_then(|()| {
                let wanted = self.ensure_volumes(start, &scope, None, started)?;
                self.check_running_volumes(start, &scope, wanted.as_ref(), before, started)
            });
            return unchecked_owners(checked, started);
        }
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
        let wanted_owner = self.ensure_volumes(start, &scope, image_config.as_ref(), started)?;
        let checked =
            self.check_running_volumes(start, &scope, wanted_owner.as_ref(), before, started);
        unchecked_owners(checked, started)?;
        for name in start.processes {
            if start.cancel.is_cancelled() {
                return Err(Halt::Cancelled);
            }
            let Some(process) = app.processes.iter().find(|process| &process.name == name) else {
                continue;
            };
            let service = app::service(app, name)?;
            // The environment is resolved before the old container is
            // removed, so a resolver that fails leaves it running.
            let environment =
                crate::env::resolve_required(start.env, &app.name, process, start.deadline)
                    .map_err(|source| PhaseError::Environment {
                        app: app.name.clone(),
                        source,
                    })?;
            let plan_hash = self.planned_hash(&scope, app, process, environment.as_ref())?;
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
            before(Before::Container(name))?;
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
            let referenced = || Referenced {
                environment: environment.as_ref(),
                reference: &refer,
                cleared: &cleared,
            };
            let launch = Launch {
                start,
                scope: &scope,
                dir: &dir,
                file: &file,
                service: &service,
                name,
                process,
                plan_hash,
                secret_files: owner.is_some(),
            };
            let volumes: BTreeSet<VolumeKey> = if wanted_owner.is_some() {
                app::process_volume_keys(app, std::slice::from_ref(name))
            } else {
                BTreeSet::new()
            };
            if let Some(wanted) = wanted_owner.as_ref().filter(|_| !volumes.is_empty())
                && !launch.create_and_own(
                    referenced(),
                    remaining,
                    (wanted, &volumes),
                    before,
                    started,
                )?
            {
                return Ok(());
            }
            let up = launch.compose(referenced(), AppStep::Start, left(start.deadline));
            // `compose up` may have created the container before it failed,
            // so the container is reported, and stopped, whatever it said.
            let inspected = inspect_named(&scope, &service).map_err(|error| halt(&error))?;
            if volumes.is_empty()
                && let Some(inspected) = &inspected
            {
                launch.report(inspected, started)?;
            }
            if !launch.settled(up, started)? {
                return Ok(());
            }
            let Some(inspected) = inspected else {
                started.problem = Some((name.clone(), StartProblem::exited(None, None)));
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

    /// Creates the workspace's volumes that `start.app` mounts into
    /// `started`, removes the app's leftover helpers, and returns the
    /// owner that the app wants for the volumes of the processes it
    /// starts, or `None` when they keep their owner. The owner is settled,
    /// and the helper's image pulled, before any volume is created and any
    /// container removed, so a user that cannot be resolved or a pull that
    /// fails leaves everything as it was.
    fn ensure_volumes(
        &self,
        start: &ContainerStart<'_>,
        scope: &Scope,
        image_config: Option<&ImageConfig>,
        started: &mut ContainersStarted,
    ) -> Result<Option<VolumeOwner>, Halt> {
        let app = start.app;
        let keys = app::volume_keys(app);
        volume_owner::remove_helpers(start.workspace, Some(&app.name))
            .map_err(|error| halt(&error))?;
        if keys.is_empty() {
            return Ok(None);
        }
        // The plan's owner needs no reading when only running processes
        // mount volumes and the record names each of them.
        let needs_plan = !start.processes.is_empty()
            || running_volumes(start, scope)?
                .iter()
                .any(|(_, name)| !start.owners.contains_key(name));
        let wanted = if needs_plan {
            self.volume_owner(start, scope, image_config)?
        } else {
            None
        };
        if wanted.is_some() && !app::process_volume_keys(app, start.processes).is_empty() {
            ensure_image(&self.helper_image, left(start.build), start.cancel)
                .map_err(|error| halt(&error))?;
        }
        for key in keys {
            ensure_volume(scope, &key).map_err(|error| halt(&error))?;
            let name = volume_docker_name(scope, &key)?;
            // A volume that this start gives no owner keeps its record,
            // unless the app now leaves its volumes alone.
            let kept = if needs_plan && wanted.is_none() {
                None
            } else {
                start.owners.get(&name).cloned()
            };
            started.volumes.push((name, kept));
        }
        Ok(wanted)
    }

    /// Checks the owner of each volume that only processes `up` leaves
    /// running mount. A running process keeps the owner its container
    /// started with, so such a volume gets the recorded owner, and the
    /// `wanted` owner from the plan only when the record names none. No
    /// creation of a container follows, so Docker copies nothing onto
    /// such a volume.
    fn check_running_volumes(
        &self,
        start: &ContainerStart<'_>,
        scope: &Scope,
        wanted: Option<&VolumeOwner>,
        before: &mut dyn FnMut(Before<'_>) -> Result<(), PhaseError>,
        started: &mut ContainersStarted,
    ) -> Result<(), Halt> {
        let mut pulled = false;
        for (key, name) in running_volumes(start, scope)? {
            let Some(owner) = start.owners.get(&name).or(wanted) else {
                continue;
            };
            if !pulled {
                ensure_image(&self.helper_image, left(start.build), start.cancel)
                    .map_err(|error| halt(&error))?;
                pulled = true;
            }
            give_volume(start, scope, &key, owner, before, started)?;
        }
        Ok(())
    }

    /// Returns the owner that the volumes of `start.app` should have, or
    /// `None` when they keep their owner, resolving a user's name in the
    /// app's image as [`volume_owner::resolve`] does.
    fn volume_owner(
        &self,
        start: &ContainerStart<'_>,
        scope: &Scope,
        image_config: Option<&ImageConfig>,
    ) -> Result<Option<VolumeOwner>, Halt> {
        let app = start.app;
        let Source::Image { image } = &app.source else {
            return Ok(None);
        };
        let fetched;
        let image_user = match image_config {
            Some(config) => config.user(),
            None if volume_owner::needs_image_user(app) => {
                fetched = objects::image_config(image.as_str()).map_err(|error| halt(&error))?;
                fetched.user()
            }
            None => "",
        };
        let run_as = match volume_owner::wanted(app, image_user) {
            volume_owner::Wanted::Skip => return Ok(None),
            volume_owner::Wanted::Owner(owner) => return Ok(Some(owner)),
            volume_owner::Wanted::Resolve(run_as) => run_as,
        };
        let resolved =
            volume_owner::resolve(scope, image.as_str(), &run_as, start.deadline, start.cancel)
                .map_err(|error| halt(&error))?;
        resolved
            .map(Some)
            .map_err(|problem| Halt::Failed(run_as.error(app.name.clone(), problem)))
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
            // A wait that no read after the grace ends before the deadline
            // is not ready, and the match below makes it uncertain when the
            // operation's deadline came before the process's ready timeout.
            None => watch_grace(
                began + self.start_grace,
                deadline,
                &mut ended,
                &|| watcher.running_since(),
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
            Outcome::Ended => Readiness::Failed(StartProblem::exited(watcher.code(), None)),
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
/// `ended` reports the container ended or `cancel` is set, and an end
/// that `ended` reports wins over a read that found the container running.
/// It returns [`Outcome::NotReady`] once `deadline` passes with no such
/// read, which is also how it ends when every read of a Docker that keeps
/// failing fails.
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
            docker_line,
        } if files && message.contains("/bin/sh") => ContainerError::Failed {
            operation,
            service,
            message: format!("{message}; {NO_SHELL_HINT}"),
            cause,
            docker_line,
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
        .and_then(|status| {
            script_ended(inspected, "secret write", status, |code| match code {
                secrets::NO_LENGTH => Some(format!(
                    "the length of the secret {name} did not reach the stage script"
                )),
                secrets::SHORT_FEED => Some(format!(
                    "the secret {name} arrived with fewer or more bytes than its length"
                )),
                _ => None,
            })
        });
        if staged.is_err() {
            break;
        }
    }
    let removed = delivery.removed.join(", ");
    let committed = staged.and_then(|()| {
        let status = secret_exec(
            &secrets::commit(id, names.iter(), delivery.removed.iter()),
            None,
            delivery.docker_environment,
            "secret commit",
            (left(deadline), cancel),
        )?;
        script_ended(inspected, "secret commit", status, |code| {
            (code == secrets::REMOVAL_REMAINED).then(|| {
                format!(
                    "the file of one of the lapsed optional secrets {removed} could not be removed and remains in {}",
                    secrets::SECRETS_DIR
                )
            })
        })
    });
    committed.map_err(|error| {
        // The discard runs even after the caller cancelled, so it has a
        // cancel flag and a deadline of its own.
        let _ = secret_exec(
            &secrets::discard(id),
            None,
            delivery.docker_environment,
            "secret discard",
            (DOCKER_DEADLINE, &Cancel::new()),
        );
        if delivery_starting {
            ended_early(inspected, &names).unwrap_or(error)
        } else {
            error
        }
    })
}

/// The exit statuses that `docker exec` gives when it cannot run the
/// process itself: 125 when Docker's own run fails, 126 when the process
/// cannot be invoked, and 127 when it is not found.
const EXEC_OWN_STATUSES: [i32; 3] = [125, 126, 127];

/// Returns `Ok` when the `docker exec` of a secret script for `operation`
/// on the container `inspected` exited with `status` 0, and its failure
/// otherwise.
///
/// The failure's message is phase's own sentence, chosen from the exit
/// status and from Docker's view of the container afterwards, and never
/// from what the exec printed, which [`secret_exec`] discards. `own` gives
/// the sentence for a status that the script itself exits with, such as
/// [`secrets::SHORT_FEED`]. A status of `docker exec` itself, one of
/// [`EXEC_OWN_STATUSES`], says that Docker could not run the script. Any
/// other status, such as 1, may be the script's or Docker's, so phase
/// inspects the container: one that Docker no longer has, or that no
/// longer runs, explains the failure, and otherwise the message names the
/// status.
///
/// # Errors
///
/// Returns the errors of checking that docker answers, and
/// [`ContainerError::Failed`] for any status other than 0.
fn script_ended(
    inspected: &Inspected,
    operation: &'static str,
    status: std::process::ExitStatus,
    own: impl Fn(i32) -> Option<String>,
) -> Result<(), ContainerError> {
    if status.success() {
        return Ok(());
    }
    check()?;
    let code = status.code();
    let message = match code.and_then(&own) {
        Some(said) => said,
        None => match code {
            Some(code) if EXEC_OWN_STATUSES.contains(&code) => {
                format!("docker exec could not run the script and exited with status {code}")
            }
            _ => match reinspect(&inspected.container) {
                Ok(None) => "docker no longer has the container".to_owned(),
                Ok(Some(fresh)) if !fresh.state.runs() => {
                    "the container stopped before the script finished".to_owned()
                }
                _ => code.map_or_else(
                    || "a signal ended the script".to_owned(),
                    |code| format!("the script exited with status {code}"),
                ),
            },
        },
    };
    Err(ContainerError::failed(
        operation,
        Some(inspected.container.service().as_str()),
        message,
    ))
}

/// Returns the failure of a container `inspected` that ended before its
/// secret files were written, which explains a start's write that failed:
/// phase's own sentence for the wrapper's status [`secrets::WRAPPER_FAILED`],
/// which names the secrets of `names`, and [`NO_SHELL_HINT`] when Docker
/// could not run the wrapper. It returns `None` when the container still
/// runs. Only a start calls it, as [`Delivery::starting`] says.
///
/// The message never holds a line of the container's log. The wrapper
/// runs under the image's own `/bin/sh`, and some secret files may already
/// be written when it exits, so a hostile shell could print a value in a
/// line that looks like the wrapper's.
fn ended_early(inspected: &Inspected, names: &[String]) -> Option<ContainerError> {
    let fresh = reinspect(&inspected.container).ok()??;
    let ContainerState::Exited(code) = fresh.state else {
        return None;
    };
    Some(ContainerError::failed(
        "secret write",
        Some(inspected.container.service().as_str()),
        ended_message(code, names),
    ))
}

/// Returns phase's sentence for a container that exited with `code` before
/// the secret files of `names` were written.
fn ended_message(code: i32, names: &[String]) -> String {
    match code {
        secrets::NO_SHELL => format!("the container exited with status {code}; {NO_SHELL_HINT}"),
        secrets::WRAPPER_FAILED => format!(
            "the container exited with status {code} before its secrets were written: its wrapper gave up waiting for the secret files {}, or the image has no sleep program or no command to run",
            names.join(", ")
        ),
        code => format!("the container exited with status {code} before its secrets were written"),
    }
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
        before: &mut dyn FnMut(Before<'_>) -> Result<(), PhaseError>,
    ) -> ContainersStarted {
        let mut started = ContainersStarted::default();
        match self.start_into(start, before, &mut started) {
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
            // A container without a recorded plan hash is not one that
            // phase finished starting, so `start` replaces it. `rewrite`
            // compares the hash, because it needs the process's resolved
            // environment.
            if entry.plan_hash.is_none() {
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

    fn started_at(
        &self,
        workspace: &WorkspaceId,
        app: &AppName,
        containers: &[ContainerEntry],
    ) -> Result<Vec<(ProcessName, String)>, PhaseError> {
        let scope = self.scope(workspace, app);
        let mut started = Vec::new();
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
            match inspect_id(&scope, &service, &id) {
                Ok(Some(inspected)) => started.push((entry.process.clone(), inspected.started)),
                Ok(None) | Err(ContainerError::Foreign { .. }) => {}
                Err(error) => return Err(docker_error(&error)),
            }
        }
        Ok(started)
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
        let scope = self.scope(rewrite.workspace, &app.name);
        // A start writes secret files only into a container of an image.
        if !matches!(app.source, Source::Image { .. })
            || app::file_secrets(app, process).next().is_none()
        {
            // A process without secret files is compared with a fresh
            // environment. One that cannot be resolved makes `up` replace
            // the container, and the start then reports the resolver's
            // error and leaves the old container running.
            let Ok(environment) =
                crate::env::resolve_required(rewrite.env, &app.name, process, rewrite.deadline)
            else {
                return Ok(Rewritten::Replace);
            };
            return Ok(
                match self.planned_hash(&scope, app, process, environment.as_ref()) {
                    Ok(planned) if rewrite.entry.plan_hash.as_ref() == Some(&planned) => {
                        Rewritten::NoFiles
                    }
                    Ok(_) => Rewritten::Replace,
                    Err(error @ PhaseError::StateUnwritable { .. }) => return Err(error),
                    Err(_) => Rewritten::Replace,
                },
            );
        }
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
        // A container that phase started from another plan of its
        // process, or with other values of its variables that are not
        // secrets, is not the one that the plan asks for, so `up` replaces
        // it before it writes any file.
        match self.planned_hash(&scope, app, process, environment.as_ref()) {
            Ok(planned) if rewrite.entry.plan_hash.as_ref() == Some(&planned) => {}
            Ok(_) => return Ok(Rewritten::Replace),
            Err(error @ PhaseError::StateUnwritable { .. }) => return Err(error),
            Err(error) => {
                return Ok(Rewritten::Kept {
                    reason: error.to_string(),
                });
            }
        }
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
        let stopped = stop
            .containers
            .iter()
            .rev()
            .map(|entry| {
                (
                    entry.process.clone(),
                    stop_one(&scope, entry, stop.deadline),
                )
            })
            .collect();
        // A volume-owner helper that an interrupted `up` left behind is
        // removed too, so it does not keep a volume in use. The run record
        // does not name it, so a failure here is no container's result,
        // and the next `up` or sweep removes the helper.
        let _ = volume_owner::remove_helpers(stop.workspace, Some(stop.app));
        stopped
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
    /// exited, is restarting, or has restarted since it was created, as
    /// [`seen`] reads the state. A read that fails reports the container
    /// as not ended, and a later read reports it again.
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
        if let Ok(read) = reinspect(&self.container) {
            match seen(
                read.as_ref()
                    .map(|inspected| (inspected.state, inspected.restarts)),
            ) {
                Seen::Ended(code) => {
                    self.ended.set(true);
                    self.code.set(code);
                }
                Seen::Running => self.running_since.set(Some(at)),
                Seen::Unsure => {}
            }
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

/// What one read of a waiting container's state tells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seen {
    /// The container has ended, with the exit code that docker reported
    /// when it exited.
    Ended(Option<i32>),
    /// The container runs.
    Running,
    /// The read shows neither, so a later read decides.
    Unsure,
}

/// Returns what a read that found the container in `state` after
/// `restarts` restarts tells, or `None` when docker no longer has it. A
/// container that docker no longer has, that exited, that Docker is
/// restarting, or that Docker has restarted has ended. Docker restarts a
/// container that exits under its restart policy, so a container that
/// crashes in a loop never shows as exited, and its first exit ends the
/// wait. Only `running` counts as running. A paused container, one that
/// docker created and has not started, and one in a state phase does not
/// know, such as `removing`, prove nothing about the start grace.
fn seen(read: Option<(ContainerState, u32)>) -> Seen {
    match read {
        None => Seen::Ended(None),
        Some((ContainerState::Exited(code), _)) => Seen::Ended(Some(code)),
        Some((ContainerState::Restarting, _)) => Seen::Ended(None),
        Some((_, restarts)) if restarts > 0 => Seen::Ended(None),
        Some((ContainerState::Running, _)) => Seen::Running,
        Some((ContainerState::Paused | ContainerState::Created | ContainerState::Unknown, _)) => {
            Seen::Unsure
        }
    }
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
