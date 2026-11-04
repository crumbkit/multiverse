//! The containers of a workspace's app: the store id and the scope of the
//! workspace's containers, starting one, waiting for its readiness,
//! following its log, and reporting its state and volumes.
//!
//! A workspace calls docker only once it has run a container. The first
//! `up` that starts one writes `<runs>/<workspace>/.containers`, and every
//! later command of the workspace asks docker for its containers whatever
//! the manifest declares, so a container that the manifest dropped while
//! it ran is still found, reported and stopped. A workspace without the
//! file has no container, so its commands never run docker, and a missing
//! docker or a daemon that does not answer does not stop them. For a
//! workspace with the file, either one fails the command.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{BuildHasher as _, RandomState};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::clock::{FOLLOW_INTERVAL, pause};
use crate::docker::{self, ContainerState, Inspected, Scope};
use crate::error::Error;
use crate::manifest::{Container, Manifest};
use crate::name::WorkspaceName;
use crate::ports;
use crate::process::{self, Launch};
use crate::record::Workspace;
use crate::runs::{
    Kind, ProcessState, ProcessStatus, RUN_VERSION, RunRecord, Runs, START_GRACE, TAIL_LINES,
    UpOutcome, Versioned, owns_group, read_json, remove_if_present, tail, write_json,
};
use crate::store;

/// The file in the runs directory that holds the store id.
const STORE_ID_FILE: &str = ".store-id";

/// The file in a workspace's run directory that says it has run a
/// container.
const CONTAINERS_FILE: &str = ".containers";

/// The file in a workspace's run directory that names the anonymous
/// volumes of the containers that `down` removed.
const ANONYMOUS_FILE: &str = ".anonymous-volumes";

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
    /// `None` when the workspace has never run a container.
    pub(super) fn scope(&self, name: &WorkspaceName) -> Result<Option<Scope>, Error> {
        if !self.workspace_dir(name).join(CONTAINERS_FILE).exists() {
            return Ok(None);
        }
        Ok(self.store_id()?.map(|id| Scope::new(&id, name)))
    }

    /// Returns the scope of the containers of the workspace `name`, and
    /// records that the workspace runs containers.
    pub(super) fn ensure_scope(&self, name: &WorkspaceName) -> Result<Scope, Error> {
        let id = self.ensure_store_id()?;
        let marker = self.workspace_dir(name).join(CONTAINERS_FILE);
        if !marker.exists() {
            store::replace_file(&marker, b"")?;
        }
        Ok(Scope::new(&id, name))
    }

    /// Returns the names of the containers that `manifest` declares, and
    /// the containers of the workspace that docker lists.
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

    /// Returns the containers of the workspace `name` that exist, running
    /// or stopped, as docker lists them by their labels.
    pub(super) fn existing_containers(
        &self,
        name: &WorkspaceName,
    ) -> Result<BTreeSet<String>, Error> {
        match self.scope(name)? {
            Some(scope) => Ok(docker::listed(&scope)?.into_iter().collect()),
            None => Ok(BTreeSet::new()),
        }
    }

    /// Returns the containers of the workspace `name` that docker lists,
    /// running or stopped, in name order. It returns none for a workspace
    /// that has never run a container.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DockerMissing`], [`Error::DockerUnavailable`] and
    /// [`Error::DockerFailed`].
    pub fn docker_containers(&self, name: &WorkspaceName) -> Result<Vec<String>, Error> {
        Ok(self.existing_containers(name)?.into_iter().collect())
    }

    /// Reports whether the container `container` of the workspace `name`
    /// runs.
    pub(super) fn container_runs(
        &self,
        name: &WorkspaceName,
        container: &str,
    ) -> Result<bool, Error> {
        let Some(scope) = self.scope(name)? else {
            return Ok(false);
        };
        Ok(matches!(
            docker::inspect(&scope, container)?,
            Some(Inspected {
                state: ContainerState::Running,
                ..
            })
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
        let inspected = match self.scope(&workspace.name)? {
            Some(scope) => docker::inspect(&scope, container)?.map(|inspected| (scope, inspected)),
            None => None,
        };
        let state = match &inspected {
            None => ProcessState::Stopped,
            Some((scope, inspected)) => {
                if inspected.state == ContainerState::Running
                    && !self.follower_alive(&workspace.name, container)?
                    && let Ok(_lock) = store::lock(
                        &self.workspace_dir(&workspace.name).join(".lock"),
                        Duration::ZERO,
                    )
                {
                    self.follow_logs(scope, container, inspected, true)?;
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
            group_alive: false,
        }
    }

    /// Starts one container and waits for its readiness, as
    /// [`Runs::up`] describes, or makes sure that a container that already
    /// runs has a log follower. It returns `None` when the container is
    /// ready, and the outcome that stops `up` otherwise. When a step after
    /// `docker run` fails, it removes the container before it returns the
    /// error.
    pub(super) fn start_container(
        &self,
        workspace: &Workspace,
        scope: &Scope,
        name: &str,
        declared: &Container,
    ) -> Result<Option<UpOutcome>, Error> {
        match docker::inspect(scope, name)? {
            Some(inspected) if inspected.state == ContainerState::Running => {
                if !self.follower_alive(&workspace.name, name)? {
                    self.follow_logs(scope, name, &inspected, true)?;
                }
                return Ok(None);
            }
            // A container that exited on its own, or that docker created
            // without starting, still holds its name, so it is removed
            // before the new one starts.
            Some(_) => {
                self.remove_container(&workspace.name, scope, name)?;
            }
            None => {}
        }
        let mut host_ports = Vec::new();
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
            host_ports.push(number);
        }
        let log = self.log(&workspace.name, name);
        store::rotate_log(&log, &self.file(&workspace.name, name, "log.1"))?;
        docker::start(&docker::Run {
            scope,
            name,
            container: declared,
            host_ports: &host_ports,
        })?;
        match self.wait_container(workspace, scope, name, declared) {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                let _ = self.remove_container(&workspace.name, scope, name);
                Err(error)
            }
        }
    }

    /// Follows the log of the container `name` that `docker run` just
    /// started, and waits for its readiness.
    fn wait_container(
        &self,
        workspace: &Workspace,
        scope: &Scope,
        name: &str,
        declared: &Container,
    ) -> Result<Option<UpOutcome>, Error> {
        let log = self.log(&workspace.name, name);
        if let Some(inspected) = docker::inspect(scope, name)? {
            self.follow_logs(scope, name, &inspected, false)?;
        }
        let port = declared
            .ready
            .as_ref()
            .and_then(|ready| workspace.ports.get(&ready.port).copied());
        let timeout = declared
            .ready
            .as_ref()
            .map_or(START_GRACE, |ready| ready.timeout);
        let deadline = Instant::now() + timeout;
        loop {
            let state = docker::inspect(scope, name)?.map(|inspected| inspected.state);
            if state != Some(ContainerState::Running) && state != Some(ContainerState::Created) {
                let code = match state {
                    Some(ContainerState::Exited(code)) => Some(code),
                    _ => None,
                };
                // `docker logs` may not have copied the last lines yet.
                pause(FOLLOW_INTERVAL);
                return Ok(Some(UpOutcome::Exited {
                    process: name.to_owned(),
                    code,
                    signal: None,
                    tail: tail(&log, TAIL_LINES),
                    log,
                }));
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
                    port: ready.port.clone(),
                    number: port.unwrap_or_default(),
                    timeout: ready.timeout,
                    tail: tail(&log, TAIL_LINES),
                    log,
                }));
            }
            pause(FOLLOW_INTERVAL);
        }
    }

    /// Stops and removes the container `container` of `scope`, and
    /// records its anonymous volumes, which stay until `mxv remove
    /// --delete-volumes`.
    pub(super) fn remove_container(
        &self,
        name: &WorkspaceName,
        scope: &Scope,
        container: &str,
    ) -> Result<(), Error> {
        if let Some(anonymous) = docker::remove(scope, container, self.grace)?
            && !anonymous.is_empty()
        {
            let path = self.workspace_dir(name).join(ANONYMOUS_FILE);
            let mut recorded: BTreeSet<String> = read_json(&path)?.unwrap_or_default();
            let before = recorded.len();
            recorded.extend(anonymous);
            if recorded.len() != before {
                write_json(&path, &recorded)?;
            }
        }
        remove_if_present(&self.file(name, container, "follower"))
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
    /// the named volumes that docker lists by their labels, the anonymous
    /// volumes of its containers, and the anonymous volumes that `down`
    /// recorded. It returns none for a workspace that has never run a
    /// container.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DockerMissing`], [`Error::DockerUnavailable`] and
    /// [`Error::DockerFailed`].
    pub fn volumes(&self, name: &WorkspaceName) -> Result<Vec<String>, Error> {
        let Some(scope) = self.scope(name)? else {
            return Ok(Vec::new());
        };
        let mut volumes: BTreeSet<String> = docker::volumes(&scope)?.into_iter().collect();
        for container in docker::listed(&scope)? {
            if let Some(inspected) = docker::inspect(&scope, &container)? {
                volumes.extend(inspected.anonymous);
            }
        }
        let recorded: Option<BTreeSet<String>> =
            read_json(&self.workspace_dir(name).join(ANONYMOUS_FILE))?;
        volumes.extend(recorded.unwrap_or_default());
        Ok(volumes.into_iter().collect())
    }

    /// Deletes the volumes `volumes`, which [`Runs::volumes`] returned.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DockerMissing`], [`Error::DockerUnavailable`] and
    /// [`Error::DockerFailed`].
    pub fn delete_volumes(&self, volumes: &[String]) -> Result<(), Error> {
        docker::remove_volumes(volumes)
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
