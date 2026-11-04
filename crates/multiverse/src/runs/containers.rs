//! The containers of a workspace's app: the store id and the scope of the
//! workspace's containers, starting one with Docker Compose, waiting for
//! its readiness, following its log, and reporting its state and volumes.
//!
//! The run records say which docker objects belong to a workspace.
//! `<container>.container` holds the id of the container that `up`
//! started, and `.volumes` names the volumes that its containers mounted,
//! named and anonymous. `compose.yaml` is the compose file of the
//! workspace's project, which holds no value of a container's variables.
//! Every command acts on the recorded ids and names after it checked their
//! labels, so a container that the manifest dropped while it ran is still
//! found, reported and stopped. `status`, `down` and `logs` run docker only
//! for a workspace with a container record, and `remove` only for one with
//! a container or volume record, so a missing docker or a daemon that does
//! not answer does not stop a workspace that has never run a container. For
//! a workspace with records, either one fails the command.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::hash::{BuildHasher as _, RandomState};
use std::io;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::clock::{FOLLOW_INTERVAL, pause};
use crate::docker::{self, ContainerState, Inspected, Scope};
use crate::error::Error;
use crate::manifest::{Container, Manifest, ReadyCheck};
use crate::name::WorkspaceName;
use crate::ports;
use crate::process::{self, Launch};
use crate::record::Workspace;
use crate::runs::{
    Kind, ProcessState, ProcessStatus, RUN_VERSION, RunRecord, Runs, START_GRACE, TAIL_LINES,
    UpOutcome, Versioned, Waited, owns_group, read_json, remove_if_present, tail, write_json,
};
use crate::store;

/// The file in the runs directory that holds the store id.
const STORE_ID_FILE: &str = ".store-id";

/// The extension of the file that records a container's id.
const CONTAINER_EXTENSION: &str = "container";

/// The file in a workspace's run directory that names the volumes of its
/// containers.
const VOLUMES_FILE: &str = ".volumes";

/// The compose file of a workspace's project, in its run directory.
const COMPOSE_FILE: &str = "compose.yaml";

/// The store id file: the id of the state directory, which labels its
/// containers and volumes.
#[derive(Debug, Serialize, Deserialize)]
struct StoreId {
    /// The file's version, which is [`RUN_VERSION`].
    version: u32,
    /// The id, 32 hex digits.
    id: String,
}

impl Versioned for StoreId {
    fn version(&self) -> Option<u32> {
        Some(self.version)
    }
}

/// The record of a container that `up` started.
#[derive(Debug, Serialize, Deserialize)]
struct ContainerRecord {
    /// The file's version, which is [`RUN_VERSION`].
    version: u32,
    /// The container's id.
    id: String,
}

impl Versioned for ContainerRecord {
    fn version(&self) -> Option<u32> {
        Some(self.version)
    }
}

/// The record of the volumes of a workspace's containers.
#[derive(Debug, Default, Serialize, Deserialize)]
struct VolumesRecord {
    /// The file's version, which is [`RUN_VERSION`].
    version: u32,
    /// The volumes' docker names.
    volumes: BTreeSet<String>,
}

impl Versioned for VolumesRecord {
    fn version(&self) -> Option<u32> {
        Some(self.version)
    }
}

/// Returns a new store id: 32 hex digits from two hashes that std seeds
/// with the operating system's randomness, mixed with the time and the
/// process id.
fn new_store_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let seed = (nanos, std::process::id());
    let high = RandomState::new().hash_one(seed);
    let low = RandomState::new().hash_one((seed, high));
    format!("{high:016x}{low:016x}")
}

impl Runs {
    /// Returns the store id, or `None` before the first container of any
    /// workspace started.
    fn store_id(&self) -> Result<Option<String>, Error> {
        let record: Option<StoreId> = read_json(&self.dir.join(STORE_ID_FILE))?;
        Ok(record.map(|record| record.id))
    }

    /// Returns the store id, and writes a new one when the store has none.
    /// It holds `<runs>/.store-id.lock` meanwhile, so two commands that
    /// start the store's first containers at once agree on one id.
    fn ensure_store_id(&self) -> Result<String, Error> {
        let _lock = store::lock(&self.dir.join(".store-id.lock"), self.lock_wait)?;
        if let Some(id) = self.store_id()? {
            return Ok(id);
        }
        let id = new_store_id();
        write_json(
            &self.dir.join(STORE_ID_FILE),
            &StoreId {
                version: RUN_VERSION,
                id: id.clone(),
            },
        )?;
        Ok(id)
    }

    /// Returns the scope of the containers of the workspace `name`, or
    /// `None` before the store's first container started.
    pub(super) fn scope(&self, name: &WorkspaceName) -> Result<Option<Scope>, Error> {
        Ok(self.store_id()?.map(|id| Scope::new(&id, name)))
    }

    /// Returns the scope of the containers of the workspace `name`, and
    /// writes the store id when the store has none.
    pub(super) fn ensure_scope(&self, name: &WorkspaceName) -> Result<Scope, Error> {
        Ok(Scope::new(&self.ensure_store_id()?, name))
    }

    /// Returns the names of the containers that `manifest` declares, and
    /// the containers of the workspace that the run records name.
    pub(super) fn containers(
        &self,
        workspace: &Workspace,
        manifest: Option<&Manifest>,
    ) -> Result<BTreeSet<String>, Error> {
        let mut names: BTreeSet<String> = manifest
            .map(|manifest| manifest.containers.keys().cloned().collect())
            .unwrap_or_default();
        names.extend(self.existing_containers(&workspace.name)?);
        Ok(names)
    }

    /// Returns the containers of the workspace `name` that have a record,
    /// which `up` writes when it starts one and `down` deletes when it
    /// removes it.
    pub(super) fn existing_containers(
        &self,
        name: &WorkspaceName,
    ) -> Result<BTreeSet<String>, Error> {
        let dir = self.workspace_dir(name);
        let unreadable = |source| Error::StateUnreadable {
            path: dir.clone(),
            source,
        };
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
            Err(error) => return Err(unreadable(error)),
        };
        let mut names = BTreeSet::new();
        for entry in entries {
            let path = entry.map_err(unreadable)?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == CONTAINER_EXTENSION)
                && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
                && !stem.starts_with('.')
            {
                names.insert(stem.to_owned());
            }
        }
        Ok(names)
    }

    /// Returns the containers of the workspace `name` that have a record,
    /// in name order. It runs no docker command.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StateUnreadable`] when the run directory cannot be
    /// read.
    pub fn docker_containers(&self, name: &WorkspaceName) -> Result<Vec<String>, Error> {
        Ok(self.existing_containers(name)?.into_iter().collect())
    }

    /// Returns the recorded id of the container `container`.
    fn container_id(&self, name: &WorkspaceName, container: &str) -> Result<Option<String>, Error> {
        let record: Option<ContainerRecord> =
            read_json(&self.file(name, container, CONTAINER_EXTENSION))?;
        Ok(record.map(|record| record.id))
    }

    /// Returns the scope and what docker reports about the recorded
    /// container `container`, or `None` when it has no record or docker no
    /// longer has it.
    fn inspect_recorded(
        &self,
        name: &WorkspaceName,
        container: &str,
    ) -> Result<Option<(Scope, Inspected)>, Error> {
        let Some(id) = self.container_id(name, container)? else {
            return Ok(None);
        };
        let Some(scope) = self.scope(name)? else {
            return Ok(None);
        };
        Ok(docker::inspect(&scope, container, &id)?.map(|inspected| (scope, inspected)))
    }

    /// Reports whether the container `container` of the workspace `name`
    /// runs.
    pub(super) fn container_runs(
        &self,
        name: &WorkspaceName,
        container: &str,
    ) -> Result<bool, Error> {
        Ok(matches!(
            self.inspect_recorded(name, container)?,
            Some((
                _,
                Inspected {
                    state: ContainerState::Running,
                    ..
                }
            ))
        ))
    }

    /// Returns the containers of the workspace `name` that run, in name
    /// order.
    pub(super) fn running_containers(&self, name: &WorkspaceName) -> Result<Vec<String>, Error> {
        let mut running = Vec::new();
        for container in self.existing_containers(name)? {
            if self.container_runs(name, &container)? {
                running.push(container);
            }
        }
        Ok(running)
    }

    /// Returns the status of the container `container`, which docker
    /// reports. When the container runs and its log follower has ended,
    /// such as after a restart outside mxv, it starts a new follower,
    /// unless another command holds the workspace's lock.
    pub(super) fn container_status(
        &self,
        workspace: &Workspace,
        manifest: Option<&Manifest>,
        container: &str,
    ) -> Result<ProcessStatus, Error> {
        let state = match self.inspect_recorded(&workspace.name, container)? {
            None => ProcessState::Stopped,
            Some((scope, inspected)) => {
                if inspected.state == ContainerState::Running
                    && !self.follower_alive(&workspace.name, container)?
                    && let Ok(_lock) = store::lock(
                        &self.workspace_dir(&workspace.name).join(".lock"),
                        Duration::ZERO,
                    )
                {
                    self.follow_logs(&scope, container, &inspected, true)?;
                }
                container_state(inspected.state)
            }
        };
        Ok(self.container_status_of(workspace, manifest, container, state))
    }

    /// Returns the status of the container `container` in `state`.
    pub(super) fn container_status_of(
        &self,
        workspace: &Workspace,
        manifest: Option<&Manifest>,
        container: &str,
        state: ProcessState,
    ) -> ProcessStatus {
        let ports: BTreeMap<String, u16> = manifest
            .and_then(|manifest| manifest.containers.get(container))
            .map(|declared| {
                declared
                    .ports
                    .iter()
                    .filter_map(|published| {
                        workspace
                            .ports
                            .get(&published.port)
                            .map(|number| (published.port.clone(), *number))
                    })
                    .collect()
            })
            .unwrap_or_default();
        ProcessStatus {
            name: container.to_owned(),
            kind: Kind::Container,
            state,
            ports,
            log: self.log(&workspace.name, container),
            group_alive: None,
        }
    }

    /// Starts the container `name` of `manifest` and waits for its
    /// readiness, as [`Runs::up`] describes, or makes sure that a container
    /// that already runs has a log follower. It returns `None` when the
    /// container is ready, and the outcome that stops `up` otherwise. When
    /// a step after `docker compose up` fails, it removes the container
    /// before it returns the error.
    pub(super) fn start_container(
        &self,
        workspace: &Workspace,
        scope: &Scope,
        manifest: &Manifest,
        name: &str,
        declared: &Container,
    ) -> Result<Option<UpOutcome>, Error> {
        match self.inspect_recorded(&workspace.name, name)? {
            Some((_, inspected)) if inspected.state == ContainerState::Running => {
                if !self.follower_alive(&workspace.name, name)? {
                    self.follow_logs(scope, name, &inspected, true)?;
                }
                return Ok(None);
            }
            // A container that exited on its own, or that docker created
            // without starting, still holds its name, so it is removed
            // before the new one starts.
            Some(_) => self.remove_container(&workspace.name, scope, name)?,
            None => self.forget_container(&workspace.name, name)?,
        }
        // A container of the scope that has the name and no record, such
        // as one that an earlier `up` started before it was stopped, is
        // removed too. One with other labels fails as foreign.
        let named = scope.container_name(name);
        if let Some(anonymous) = docker::remove(scope, name, &named, self.grace)? {
            self.record_volumes(&workspace.name, anonymous)?;
        }
        for published in &declared.ports {
            let Some(number) = workspace.ports.get(&published.port).copied() else {
                continue;
            };
            if ports::is_taken(number) {
                return Ok(Some(UpOutcome::PortTaken {
                    process: name.to_owned(),
                    port: published.port.clone(),
                    number,
                    holders: Vec::new(),
                }));
            }
        }
        for volume in declared.volumes.keys() {
            docker::check_volume(scope, volume)?;
        }
        self.record_volumes(
            &workspace.name,
            declared
                .volumes
                .keys()
                .map(|volume| scope.volume_name(volume)),
        )?;
        let log = self.log(&workspace.name, name);
        store::rotate_log(&log, &self.file(&workspace.name, name, "log.1"))?;
        let dir = self.workspace_dir(&workspace.name);
        let file = dir.join(COMPOSE_FILE);
        write_json(
            &file,
            &docker::compose_file(scope, &manifest.containers, &workspace.ports),
        )?;
        let started = docker::compose_up(
            scope,
            &dir,
            &file,
            name,
            &docker::compose_env(&manifest.containers, name),
        );
        // `compose up` may have created the container before it failed, so
        // the container is recorded and removed whatever it reported.
        let recorded = docker::inspect(scope, name, &named).and_then(|inspected| {
            if let Some(inspected) = &inspected {
                write_json(
                    &self.file(&workspace.name, name, CONTAINER_EXTENSION),
                    &ContainerRecord {
                        version: RUN_VERSION,
                        id: inspected.id.clone(),
                    },
                )?;
            }
            Ok(inspected)
        });
        match started {
            Ok(docker::Up::Started) => {}
            Ok(docker::Up::PortAllocated(number)) => {
                // docker publishes the port for a container that the port
                // check before `compose up` could not see, such as one
                // that is created and not started.
                let _ = self.remove_container(&workspace.name, scope, name);
                let published = declared
                    .ports
                    .iter()
                    .find(|published| {
                        number.is_none() || workspace.ports.get(&published.port).copied() == number
                    })
                    .or_else(|| declared.ports.first());
                let port = published
                    .map(|published| published.port.clone())
                    .unwrap_or_default();
                return Ok(Some(UpOutcome::PortTaken {
                    number: number
                        .or_else(|| workspace.ports.get(&port).copied())
                        .unwrap_or_default(),
                    process: name.to_owned(),
                    port,
                    holders: Vec::new(),
                }));
            }
            Err(error) => {
                let _ = self.remove_container(&workspace.name, scope, name);
                return Err(error);
            }
        }
        let Some(inspected) = recorded? else {
            return Ok(Some(self.exited(&workspace.name, name, None)));
        };
        match self.wait_container(workspace, scope, name, declared, &inspected) {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                let _ = self.remove_container(&workspace.name, scope, name);
                Err(error)
            }
        }
    }

    /// Returns the outcome of the container `name` that stopped before it
    /// was ready, with docker's exit code `code`.
    fn exited(&self, name: &WorkspaceName, container: &str, code: Option<i32>) -> UpOutcome {
        let log = self.log(name, container);
        // `docker logs` may not have copied the last lines yet.
        pause(FOLLOW_INTERVAL);
        UpOutcome::Exited {
            process: container.to_owned(),
            code,
            signal: None,
            tail: tail(&log, TAIL_LINES),
            log,
        }
    }

    /// Follows the log of the container `name` that `docker compose up`
    /// just started, and waits for its readiness.
    fn wait_container(
        &self,
        workspace: &Workspace,
        scope: &Scope,
        name: &str,
        declared: &Container,
        inspected: &Inspected,
    ) -> Result<Option<UpOutcome>, Error> {
        let log = self.log(&workspace.name, name);
        self.follow_logs(scope, name, inspected, false)?;
        let ready_port = declared
            .ready
            .as_ref()
            .and_then(|ready| match &ready.check {
                ReadyCheck::Port(port) => Some(port),
                ReadyCheck::Log(_) => None,
            });
        let port = ready_port.and_then(|ready_port| workspace.ports.get(ready_port).copied());
        let timeout = declared
            .ready
            .as_ref()
            .map_or(START_GRACE, |ready| ready.timeout);
        let deadline = Instant::now() + timeout;
        loop {
            let state =
                docker::inspect(scope, name, &inspected.id)?.map(|inspected| inspected.state);
            if state != Some(ContainerState::Running) && state != Some(ContainerState::Created) {
                let code = match state {
                    Some(ContainerState::Exited(code)) => Some(code),
                    _ => None,
                };
                return Ok(Some(self.exited(&workspace.name, name, code)));
            }
            if port.is_some_and(docker::answers) {
                return Ok(None);
            }
            if Instant::now() >= deadline {
                let Some(ready) = &declared.ready else {
                    return Ok(None);
                };
                self.remove_container(&workspace.name, scope, name)?;
                return Ok(Some(UpOutcome::NotReady {
                    process: name.to_owned(),
                    waited: Waited::Port {
                        port: ready_port.cloned().unwrap_or_default(),
                        number: port.unwrap_or_default(),
                    },
                    timeout: ready.timeout,
                    tail: tail(&log, TAIL_LINES),
                    log,
                }));
            }
            pause(FOLLOW_INTERVAL);
        }
    }

    /// Adds `volumes` to the record of the workspace's volumes.
    fn record_volumes(
        &self,
        name: &WorkspaceName,
        volumes: impl IntoIterator<Item = String>,
    ) -> Result<(), Error> {
        let path = self.workspace_dir(name).join(VOLUMES_FILE);
        let mut record: VolumesRecord = read_json(&path)?.unwrap_or_default();
        let before = record.volumes.len();
        record.volumes.extend(volumes);
        if record.volumes.len() == before {
            return Ok(());
        }
        record.version = RUN_VERSION;
        write_json(&path, &record)
    }

    /// Stops and removes the recorded container `container` of `scope`,
    /// records its anonymous volumes, which stay until `mxv remove
    /// --delete-volumes`, and deletes its record.
    pub(super) fn remove_container(
        &self,
        name: &WorkspaceName,
        scope: &Scope,
        container: &str,
    ) -> Result<(), Error> {
        if let Some(id) = self.container_id(name, container)?
            && let Some(anonymous) = docker::remove(scope, container, &id, self.grace)?
        {
            self.record_volumes(name, anonymous)?;
        }
        self.forget_container(name, container)
    }

    /// Deletes the record and the follower record of the container
    /// `container`.
    fn forget_container(&self, name: &WorkspaceName, container: &str) -> Result<(), Error> {
        remove_if_present(&self.file(name, container, CONTAINER_EXTENSION))?;
        remove_if_present(&self.file(name, container, "follower"))
    }

    /// Removes the recorded containers of the workspace `name`, for
    /// `forget`. A workspace without container records runs no docker
    /// command.
    pub(super) fn remove_project(&self, name: &WorkspaceName) -> Result<(), Error> {
        let containers = self.existing_containers(name)?;
        if containers.is_empty() {
            return Ok(());
        }
        let Some(scope) = self.scope(name)? else {
            return Ok(());
        };
        for container in containers {
            self.remove_container(name, &scope, &container)?;
        }
        Ok(())
    }

    /// Reports whether the log of the container `container` may still
    /// grow, for `mxv logs -f`: its follower runs, or the container runs
    /// and this call starts a new follower, such as after the old one was
    /// killed. Another command that holds the workspace's lock may start
    /// the follower instead, so the container still counts as writing.
    pub(super) fn container_writing(
        &self,
        name: &WorkspaceName,
        container: &str,
    ) -> Result<bool, Error> {
        if self.follower_alive(name, container)? {
            return Ok(true);
        }
        let Some((scope, inspected)) = self.inspect_recorded(name, container)? else {
            return Ok(false);
        };
        if inspected.state != ContainerState::Running {
            return Ok(false);
        }
        if let Ok(_lock) = store::lock(&self.workspace_dir(name).join(".lock"), Duration::ZERO) {
            self.follow_logs(&scope, container, &inspected, true)?;
        }
        Ok(true)
    }

    /// Reports whether the log follower of the container `container` runs.
    pub(super) fn follower_alive(
        &self,
        name: &WorkspaceName,
        container: &str,
    ) -> Result<bool, Error> {
        let record: Option<RunRecord> = read_json(&self.file(name, container, "follower"))?;
        match record {
            Some(record) => owns_group(&record),
            None => Ok(false),
        }
    }

    /// Starts a detached `docker logs --follow` that appends the output of
    /// the container `inspected` to its log until the container stops, and
    /// records the follower's process id in `<container>.follower`. With
    /// `since` the follower starts at the container's last start, which
    /// skips the lines that an earlier follower copied.
    fn follow_logs(
        &self,
        scope: &Scope,
        container: &str,
        inspected: &Inspected,
        since: bool,
    ) -> Result<(), Error> {
        let name = scope.workspace();
        let exit_file = self.file(name, container, "logs-exit");
        remove_if_present(&exit_file)?;
        let argv = docker::follow_args(&inspected.id, since.then_some(inspected.started.as_str()));
        // The follower runs in a process group of its own and ends when
        // the container stops, so mxv does not wait for it.
        let follower = process::launch(&Launch {
            name: container,
            argv: &argv,
            cwd: &self.workspace_dir(name),
            env: &[],
            log: &self.log(name, container),
            exit_file: &exit_file,
        })?;
        let pid = follower.pid();
        let started = process::start_time(pid)?.unwrap_or_default();
        write_json(
            &self.file(name, container, "follower"),
            &RunRecord {
                version: RUN_VERSION,
                pid,
                started,
                ports: BTreeMap::new(),
            },
        )
    }

    /// Returns the volumes of the workspace `name` that a removal deletes:
    /// the recorded volumes that docker still has, and the anonymous
    /// volumes of its recorded containers. It runs no docker command for a
    /// workspace without records of containers or volumes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DockerMissing`], [`Error::DockerUnavailable`] and
    /// [`Error::DockerFailed`].
    pub fn volumes(&self, name: &WorkspaceName) -> Result<Vec<String>, Error> {
        let record: Option<VolumesRecord> =
            read_json(&self.workspace_dir(name).join(VOLUMES_FILE))?;
        let containers = self.existing_containers(name)?;
        if record.is_none() && containers.is_empty() {
            return Ok(Vec::new());
        }
        let Some(scope) = self.scope(name)? else {
            return Ok(Vec::new());
        };
        let mut volumes = BTreeSet::new();
        for volume in record.map(|record| record.volumes).unwrap_or_default() {
            if docker::volume_exists(&scope, &volume)? {
                volumes.insert(volume);
            }
        }
        for container in containers {
            if let Some((_, inspected)) = self.inspect_recorded(name, &container)? {
                volumes.extend(inspected.anonymous);
            }
        }
        Ok(volumes.into_iter().collect())
    }

    /// Deletes the volumes `volumes` of the workspace `name`, which
    /// [`Runs::volumes`] returned, after it checked the labels of each
    /// named volume.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DockerForeign`], [`Error::DockerMissing`],
    /// [`Error::DockerUnavailable`] and [`Error::DockerFailed`].
    pub fn delete_volumes(&self, name: &WorkspaceName, volumes: &[String]) -> Result<(), Error> {
        if volumes.is_empty() {
            return Ok(());
        }
        match self.scope(name)? {
            Some(scope) => docker::remove_volumes(&scope, volumes),
            None => Ok(()),
        }
    }
}

/// Returns the state that mxv reports for a container in `state`. The
/// exit code is docker's, which mxv does not read as a signal.
const fn container_state(state: ContainerState) -> ProcessState {
    match state {
        ContainerState::Running => ProcessState::Running { pid: None },
        ContainerState::Created => ProcessState::Created,
        ContainerState::Exited(code) => ProcessState::Exited {
            code: Some(code),
            signal: None,
        },
    }
}
