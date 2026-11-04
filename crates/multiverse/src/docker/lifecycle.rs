//! The containers of one workspace across mxv commands: the store id and
//! the scope, the run records that name the workspace's containers and
//! volumes, starting a service with Docker Compose, following its log,
//! and reporting, stopping and removing what the records name.
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
//!
//! The caller orders the services of a workspace among its other
//! processes, and waits for a started container's readiness with
//! [`super::reinspect`] and [`super::answers`].

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::hash::{BuildHasher as _, RandomState};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::docker::command::{check, check_compose};
use crate::docker::compose::{Up, compose_env, compose_file, compose_up};
use crate::docker::names::{ContainerId, Scope, ServiceName, VolumeName};
use crate::docker::objects::{
    ContainerState, FollowFiles, Inspected, OwnedContainer, delete_volume, follow_logs, inspect_id,
    inspect_named, inspect_volume, remove, stop,
};
use crate::error::Error;
use crate::manifest::Container;
use crate::name::WorkspaceName;
use crate::ports;
use crate::process;
use crate::runs::{
    RUN_VERSION, RunRecord, Versioned, owns_group, read_json, remove_if_present, write_json,
};
use crate::store;

/// The file in the runs directory that holds the store id.
const STORE_ID_FILE: &str = ".store-id";

/// The extension of the file that records a container's id.
const CONTAINER_EXTENSION: &str = "container";

/// The extension of the file that records a container's log follower.
const FOLLOWER_EXTENSION: &str = "follower";

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

/// What [`Lifecycle::start`] did.
#[derive(Debug)]
pub(crate) enum Start {
    /// The recorded container already ran, and has a log follower now.
    Running,
    /// `docker compose up` started the container, and its log follower
    /// runs. The caller waits for its readiness.
    Started(OwnedContainer),
    /// Something outside the workspace holds the host port `number` of
    /// the port `port`, so the container did not start, or `docker compose
    /// up` removed it again.
    PortTaken {
        /// The port's name.
        port: String,
        /// The host port.
        number: u16,
    },
    /// `docker compose up` succeeded, and docker no longer has the
    /// container.
    Gone,
}

/// The containers of one workspace and their run records.
#[derive(Clone, Debug)]
pub(crate) struct Lifecycle {
    /// The runs directory, which holds the store id.
    runs: PathBuf,
    /// The workspace's run directory.
    dir: PathBuf,
    /// The workspace.
    workspace: WorkspaceName,
    /// How long `docker stop` waits after SIGTERM.
    grace: Duration,
    /// How long the store id's lock waits for another command.
    lock_wait: Duration,
}

impl Lifecycle {
    /// Returns the containers of the workspace `workspace`, whose run
    /// directory is `dir` in the runs directory `runs`. A container stops
    /// with `grace` between SIGTERM and SIGKILL, and the store id's lock
    /// waits up to `lock_wait`.
    pub(crate) fn new(
        runs: &Path,
        dir: PathBuf,
        workspace: &WorkspaceName,
        grace: Duration,
        lock_wait: Duration,
    ) -> Self {
        Self {
            runs: runs.to_path_buf(),
            dir,
            workspace: workspace.clone(),
            grace,
            lock_wait,
        }
    }

    /// Returns the path of a file of `service` with `extension`.
    fn file(&self, service: &ServiceName, extension: &str) -> PathBuf {
        self.dir.join(format!("{service}.{extension}"))
    }

    /// Returns the log of `service`.
    fn log(&self, service: &ServiceName) -> PathBuf {
        self.file(service, "log")
    }

    /// Returns the store id, or `None` before the first container of any
    /// workspace started.
    fn store_id(&self) -> Result<Option<String>, Error> {
        let record: Option<StoreId> = read_json(&self.runs.join(STORE_ID_FILE))?;
        Ok(record.map(|record| record.id))
    }

    /// Returns the store id, and writes a new one when the store has none.
    /// It holds `<runs>/.store-id.lock` meanwhile, so two commands that
    /// start the store's first containers at once agree on one id.
    fn ensure_store_id(&self) -> Result<String, Error> {
        let _lock = store::lock(&self.runs.join(".store-id.lock"), self.lock_wait)?;
        if let Some(id) = self.store_id()? {
            return Ok(id);
        }
        let id = new_store_id();
        write_json(
            &self.runs.join(STORE_ID_FILE),
            &StoreId {
                version: RUN_VERSION,
                id: id.clone(),
            },
        )?;
        Ok(id)
    }

    /// Returns the scope of the workspace's containers, or `None` before
    /// the store's first container started.
    fn scope(&self) -> Result<Option<Scope>, Error> {
        Ok(self.store_id()?.map(|id| Scope::new(&id, &self.workspace)))
    }

    /// Checks that docker and its compose plugin answer, and returns the
    /// scope of the workspace's containers, after it wrote the store id
    /// when the store had none, for [`Lifecycle::start`].
    ///
    /// # Errors
    ///
    /// Returns the errors of [`super::command::check`] and [`super::command::check_compose`],
    /// [`Error::Busy`] when another command holds the store id's lock, and
    /// [`Error::Unwritable`].
    pub(crate) fn prepare(&self) -> Result<Scope, Error> {
        check()?;
        check_compose()?;
        Ok(Scope::new(&self.ensure_store_id()?, &self.workspace))
    }

    /// Returns the services of the workspace that have a container record,
    /// which `start` writes when it starts one and `stop` deletes when it
    /// removes it. It runs no docker command.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StateUnreadable`] when the run directory cannot be
    /// read.
    pub(crate) fn recorded(&self) -> Result<BTreeSet<ServiceName>, Error> {
        let unreadable = |source| Error::StateUnreadable {
            path: self.dir.clone(),
            source,
        };
        let entries = match fs::read_dir(&self.dir) {
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
                && let Ok(service) = ServiceName::new(stem)
            {
                names.insert(service);
            }
        }
        Ok(names)
    }

    /// Returns the recorded id of the container of `service`. A record
    /// whose id is not a container id names no container.
    fn recorded_id(&self, service: &ServiceName) -> Result<Option<ContainerId>, Error> {
        let record: Option<ContainerRecord> = read_json(&self.file(service, CONTAINER_EXTENSION))?;
        Ok(record.and_then(|record| ContainerId::new(&record.id).ok()))
    }

    /// Returns what docker reports about the recorded container of
    /// `service`, or `None` when it has no record or docker no longer has
    /// it.
    fn inspect_recorded(&self, service: &ServiceName) -> Result<Option<Inspected>, Error> {
        let Some(id) = self.recorded_id(service)? else {
            return Ok(None);
        };
        let Some(scope) = self.scope()? else {
            return Ok(None);
        };
        inspect_id(&scope, service, &id)
    }

    /// Reports whether the recorded container of `service` runs.
    ///
    /// # Errors
    ///
    /// Returns the errors of reading the records, [`Error::DockerForeign`],
    /// the errors of [`super::command::check`] and [`Error::DockerFailed`].
    pub(crate) fn runs(&self, service: &ServiceName) -> Result<bool, Error> {
        Ok(matches!(
            self.inspect_recorded(service)?,
            Some(Inspected {
                state: ContainerState::Running,
                ..
            })
        ))
    }

    /// Returns the services whose recorded containers run, in name order.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Lifecycle::runs`].
    pub(crate) fn running(&self) -> Result<Vec<ServiceName>, Error> {
        let mut running = Vec::new();
        for service in self.recorded()? {
            if self.runs(&service)? {
                running.push(service);
            }
        }
        Ok(running)
    }

    /// Returns the state of the recorded container of `service`, or `None`
    /// when it has no record or docker no longer has it. When the
    /// container runs and its log follower has ended, such as after a
    /// restart outside mxv, it starts a new follower, unless another
    /// command holds the workspace's lock.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Lifecycle::runs`] and of starting the
    /// follower.
    pub(crate) fn status(&self, service: &ServiceName) -> Result<Option<ContainerState>, Error> {
        let Some(inspected) = self.inspect_recorded(service)? else {
            return Ok(None);
        };
        if inspected.state == ContainerState::Running
            && !self.follower_alive(service)?
            && let Ok(_lock) = store::lock(&self.dir.join(".lock"), Duration::ZERO)
        {
            self.follow(&inspected, true)?;
        }
        Ok(Some(inspected.state))
    }

    /// Starts the container of `service`, one of `containers`, the
    /// manifest's containers, with the host ports `ports` by port name, or
    /// makes sure that a recorded container that already runs has a log
    /// follower. Before `docker compose up` it removes the recorded
    /// container and any container of the scope with the service's name,
    /// checks that its host ports are free and its named volumes are the
    /// scope's, records its volumes, moves its log to `<service>.log.1`,
    /// and writes the compose file. When a step after `docker compose up`
    /// fails, it removes the container before it returns the error.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DockerForeign`], [`Error::DockerNoAddressPool`],
    /// the errors of [`super::command::check`], [`Error::DockerFailed`], and the
    /// errors of reading and writing the run records.
    pub(crate) fn start(
        &self,
        scope: &Scope,
        containers: &BTreeMap<String, Container>,
        service: &ServiceName,
        ports: &BTreeMap<String, u16>,
    ) -> Result<Start, Error> {
        let declared = containers.get(service.as_str());
        match self.inspect_recorded(service)? {
            Some(inspected) if inspected.state == ContainerState::Running => {
                if !self.follower_alive(service)? {
                    self.follow(&inspected, true)?;
                }
                return Ok(Start::Running);
            }
            // A container that exited on its own, or that docker created
            // without starting, still holds its name, so it is removed
            // before the new one starts.
            Some(inspected) => {
                self.discard(inspected)?;
                self.forget(service)?;
            }
            None => self.forget(service)?,
        }
        // A container of the scope that has the name and no record, such
        // as one that an earlier `up` started before it was stopped, is
        // removed too. One with other labels fails as foreign.
        if let Some(named) = inspect_named(scope, service)? {
            self.discard(named)?;
        }
        let published = declared
            .map(|declared| declared.ports.as_slice())
            .unwrap_or_default();
        for port in published {
            let Some(number) = ports.get(&port.port).copied() else {
                continue;
            };
            if ports::is_taken(number) {
                return Ok(Start::PortTaken {
                    port: port.port.clone(),
                    number,
                });
            }
        }
        let volumes: Vec<&String> = declared
            .map(|declared| declared.volumes.keys().collect())
            .unwrap_or_default();
        for volume in &volumes {
            inspect_volume(scope, &scope.volume_name(volume))?;
        }
        self.record_volumes(volumes.iter().map(|volume| scope.volume_name(volume)))?;
        store::rotate_log(&self.log(service), &self.file(service, "log.1"))?;
        let file = self.dir.join(COMPOSE_FILE);
        write_json(&file, &compose_file(scope, containers, ports))?;
        let started = compose_up(
            scope,
            &self.dir,
            &file,
            service,
            compose_env(containers, service.as_str()),
        );
        // `compose up` may have created the container before it failed, so
        // the container is recorded and removed whatever it reported.
        let recorded = inspect_named(scope, service).and_then(|inspected| {
            if let Some(inspected) = &inspected {
                write_json(
                    &self.file(service, CONTAINER_EXTENSION),
                    &ContainerRecord {
                        version: RUN_VERSION,
                        id: inspected.container.id().as_str().to_owned(),
                    },
                )?;
            }
            Ok(inspected)
        });
        match started {
            Ok(Up::Started) => {}
            Ok(Up::PortAllocated(number)) => {
                // docker publishes the port for a container that the port
                // check before `compose up` could not see, such as one
                // that is created and not started.
                let _ = self.remove_recorded(scope, service);
                let port = published
                    .iter()
                    .find(|port| number.is_none() || ports.get(&port.port).copied() == number)
                    .or_else(|| published.first())
                    .map(|port| port.port.clone())
                    .unwrap_or_default();
                return Ok(Start::PortTaken {
                    number: number
                        .or_else(|| ports.get(&port).copied())
                        .unwrap_or_default(),
                    port,
                });
            }
            Err(error) => {
                let _ = self.remove_recorded(scope, service);
                return Err(error);
            }
        }
        let Some(inspected) = recorded? else {
            return Ok(Start::Gone);
        };
        if let Err(error) = self.follow(&inspected, false) {
            let _ = self.remove_recorded(scope, service);
            return Err(error);
        }
        Ok(Start::Started(inspected.container))
    }

    /// Adds `volumes` to the record of the workspace's volumes.
    fn record_volumes(&self, volumes: impl IntoIterator<Item = VolumeName>) -> Result<(), Error> {
        let path = self.dir.join(VOLUMES_FILE);
        let mut record: VolumesRecord = read_json(&path)?.unwrap_or_default();
        let before = record.volumes.len();
        record
            .volumes
            .extend(volumes.into_iter().map(|volume| volume.as_str().to_owned()));
        if record.volumes.len() == before {
            return Ok(());
        }
        record.version = RUN_VERSION;
        write_json(&path, &record)
    }

    /// Stops and removes the container `inspected`, and records its
    /// anonymous volumes, which stay until `mxv remove --delete-volumes`.
    fn discard(&self, inspected: Inspected) -> Result<(), Error> {
        stop(&inspected.container, self.grace)?;
        remove(inspected.container)?;
        self.record_volumes(inspected.anonymous)
    }

    /// Stops and removes the recorded container of `service` in `scope`,
    /// records its anonymous volumes, and deletes its record.
    fn remove_recorded(&self, scope: &Scope, service: &ServiceName) -> Result<(), Error> {
        if let Some(id) = self.recorded_id(service)?
            && let Some(inspected) = inspect_id(scope, service, &id)?
        {
            self.discard(inspected)?;
        }
        self.forget(service)
    }

    /// Stops and removes the recorded container of `service`, records its
    /// anonymous volumes, which stay until `mxv remove --delete-volumes`,
    /// and deletes its record. Before the store's first container started
    /// it does nothing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DockerForeign`], the errors of [`super::command::check`],
    /// [`Error::DockerFailed`], and the errors of reading and writing the
    /// run records.
    pub(crate) fn stop(&self, service: &ServiceName) -> Result<(), Error> {
        match self.scope()? {
            Some(scope) => self.remove_recorded(&scope, service),
            None => Ok(()),
        }
    }

    /// Deletes the record and the follower record of the container of
    /// `service`.
    fn forget(&self, service: &ServiceName) -> Result<(), Error> {
        remove_if_present(&self.file(service, CONTAINER_EXTENSION))?;
        remove_if_present(&self.file(service, FOLLOWER_EXTENSION))
    }

    /// Stops and removes every recorded container of the workspace, for
    /// `forget`. A workspace without container records runs no docker
    /// command.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Lifecycle::stop`].
    pub(crate) fn stop_all(&self) -> Result<(), Error> {
        let services = self.recorded()?;
        if services.is_empty() {
            return Ok(());
        }
        let Some(scope) = self.scope()? else {
            return Ok(());
        };
        for service in services {
            self.remove_recorded(&scope, &service)?;
        }
        Ok(())
    }

    /// Reports whether the log of `service` may still grow, for `mxv logs
    /// -f`: its follower runs, or its container runs and this call starts
    /// a new follower, such as after the old one was killed. Another
    /// command that holds the workspace's lock may start the follower
    /// instead, so the container still counts as writing.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Lifecycle::status`].
    pub(crate) fn writing(&self, service: &ServiceName) -> Result<bool, Error> {
        if self.follower_alive(service)? {
            return Ok(true);
        }
        let Some(inspected) = self.inspect_recorded(service)? else {
            return Ok(false);
        };
        if inspected.state != ContainerState::Running {
            return Ok(false);
        }
        if let Ok(_lock) = store::lock(&self.dir.join(".lock"), Duration::ZERO) {
            self.follow(&inspected, true)?;
        }
        Ok(true)
    }

    /// Reports whether the log follower of `service` runs.
    fn follower_alive(&self, service: &ServiceName) -> Result<bool, Error> {
        let record: Option<RunRecord> = read_json(&self.file(service, FOLLOWER_EXTENSION))?;
        match record {
            Some(record) => owns_group(&record),
            None => Ok(false),
        }
    }

    /// Starts a detached `docker logs --follow` that appends the output of
    /// the container `inspected` to its log until the container stops, and
    /// records the follower's process id in `<service>.follower`. With
    /// `since` the follower starts at the container's last start, which
    /// skips the lines that an earlier follower copied.
    fn follow(&self, inspected: &Inspected, since: bool) -> Result<(), Error> {
        let service = inspected.container.service();
        let exit_file = self.file(service, "logs-exit");
        remove_if_present(&exit_file)?;
        // The follower runs in a process group of its own and ends when
        // the container stops, so mxv does not wait for it.
        let follower = follow_logs(
            &inspected.container,
            since.then_some(inspected.started.as_str()),
            &FollowFiles {
                cwd: &self.dir,
                log: &self.log(service),
                exit_file: &exit_file,
            },
        )?;
        // A follower whose record cannot be written stops with its
        // handle, so mxv never leaves a follower that it does not know of.
        let pid = follower.pid();
        let started = process::start_time(pid)?.unwrap_or_default();
        write_json(
            &self.file(service, FOLLOWER_EXTENSION),
            &RunRecord {
                version: RUN_VERSION,
                pid,
                started,
                ports: BTreeMap::new(),
            },
        )?;
        follower.keep();
        Ok(())
    }

    /// Returns the volumes of the workspace that a removal deletes: the
    /// recorded volumes that docker still has and that are the scope's,
    /// and the anonymous volumes of its recorded containers. It runs no
    /// docker command for a workspace without records of containers or
    /// volumes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DockerMissing`], [`Error::DockerUnavailable`] and
    /// [`Error::DockerFailed`].
    pub(crate) fn volumes(&self) -> Result<Vec<VolumeName>, Error> {
        let record: Option<VolumesRecord> = read_json(&self.dir.join(VOLUMES_FILE))?;
        let services = self.recorded()?;
        if record.is_none() && services.is_empty() {
            return Ok(Vec::new());
        }
        let Some(scope) = self.scope()? else {
            return Ok(Vec::new());
        };
        let mut volumes = BTreeSet::new();
        for volume in record.map(|record| record.volumes).unwrap_or_default() {
            let Ok(volume) = VolumeName::new(&volume) else {
                continue;
            };
            match inspect_volume(&scope, &volume) {
                Ok(Some(_)) => {
                    volumes.insert(volume);
                }
                // A recorded volume that is not the scope's is left out,
                // so a removal never deletes it.
                Ok(None) | Err(Error::DockerForeign { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        for service in services {
            if let Some(inspected) = self.inspect_recorded(&service)? {
                volumes.extend(inspected.anonymous);
            }
        }
        Ok(volumes.into_iter().collect())
    }

    /// Deletes the volumes `volumes` of the workspace, which
    /// [`Lifecycle::volumes`] returned, after it checked the labels of each
    /// one. A volume that no longer exists counts as deleted.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DockerForeign`], [`Error::DockerMissing`],
    /// [`Error::DockerUnavailable`] and [`Error::DockerFailed`].
    pub(crate) fn delete_volumes(&self, volumes: &[VolumeName]) -> Result<(), Error> {
        if volumes.is_empty() {
            return Ok(());
        }
        let Some(scope) = self.scope()? else {
            return Ok(());
        };
        for volume in volumes {
            if let Some(owned) = inspect_volume(&scope, volume)? {
                delete_volume(owned)?;
            }
        }
        Ok(())
    }
}
