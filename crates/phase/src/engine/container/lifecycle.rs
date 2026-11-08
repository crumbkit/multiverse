//! The containers of one workspace across mxv commands: the store id and
//! the scope, the run records that name the workspace's containers and
//! volumes, following a container's log, and reporting, stopping and
//! removing what the records name. An older mxv wrote these records when it
//! started the containers, and phase's container runner writes none.
//!
//! The run records say which docker objects belong to a workspace.
//! `<container>.container` holds the id of the container that `up`
//! started, and `.volumes` names the volumes that its containers mounted,
//! named and anonymous. Every command acts on the recorded ids and names after it checked their
//! labels, so a container that the manifest dropped while it ran is still
//! found, reported and stopped. `status`, `down` and `logs` run docker only
//! for a workspace with a container record, and `remove` only for one with
//! a container or volume record, so a missing docker or a daemon that does
//! not answer does not stop a workspace that has never run a container. For
//! a workspace with records, either one fails the command.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::engine::container::command::quick;
use crate::engine::container::host::{Follow, Host, Versioned};
use crate::engine::container::names::{
    ContainerId, Identity, Scope, ServiceName, StoreId, VOLUME_LABEL, VolumeKey, VolumeName,
    WORKSPACE_ID_LABEL,
};
use crate::engine::container::objects::{
    AnonymousVolume, AnonymousVolumes, ContainerState, Inspected, Volume, delete_volume,
    follow_args, inspect_anonymous, inspect_id, inspect_volume, reinspect_volume, remove, stop,
};
use crate::engine::container::{ContainerError, RunDir, volume_owner};
use crate::name::{AppName, WorkspaceId, WorkspaceName};
use crate::phase::Phase;

#[cfg(test)]
mod tests;

/// The version of the run files that this version of the engine writes
/// and reads, for containers and for the host's processes. A run file with
/// a higher version is refused.
pub const RUN_VERSION: u32 = 1;

/// The file in the runs directory that holds the store id.
const STORE_ID_FILE: &str = ".store-id";

/// The extension of the file that records a container's id.
const CONTAINER_EXTENSION: &str = "container";

/// The extension of the file that records a container's log follower.
const FOLLOWER_EXTENSION: &str = "follower";

/// The file in a workspace's run directory that names the volumes of its
/// containers.
const VOLUMES_FILE: &str = ".volumes";

/// The store id file: the id of the state directory, which labels its
/// containers and volumes.
#[derive(Debug, Serialize, Deserialize)]
struct StoreIdRecord {
    /// The file's version, which is [`RUN_VERSION`].
    version: u32,
    /// The id.
    id: StoreId,
}

impl Versioned for StoreIdRecord {
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

/// The record of the volumes of a workspace's containers: the names of
/// its named volumes, and of the anonymous volumes that inspections of its
/// containers found.
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

/// A workspace's stable id that its host has saved in the workspace's
/// record, which only [`Lifecycle::saved_id`] returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedWorkspaceId {
    /// The id.
    id: WorkspaceId,
}

impl SavedWorkspaceId {
    /// Returns the id.
    #[must_use]
    pub const fn id(&self) -> &WorkspaceId {
        &self.id
    }
}

/// The scope of a workspace's containers, which only [`Lifecycle::existing`]
/// returns. Every method that acts on the
/// workspace's docker objects takes one, so the labels that it checks are
/// those of the store id in the runs directory. A [`Lifecycle`] refuses a
/// `Prepared` of another workspace or runs directory.
#[derive(Debug, PartialEq, Eq)]
pub struct Prepared {
    /// The run directory of the workspace that the scope names.
    dir: RunDir,
    /// The scope.
    scope: Scope,
}

/// The volumes of a workspace that a removal deletes, which
/// [`Lifecycle::volumes`] found and checked, in name order, and the scope
/// that they belong to. [`Lifecycle::delete_volumes`] refuses the volumes
/// of another scope.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct WorkspaceVolumes {
    /// The scope of the volumes, or `None` for a workspace without them.
    scope: Option<Scope>,
    /// The volumes.
    volumes: Vec<Volume>,
    /// Whether the volumes are those of the workspace's apps, which
    /// [`app_volumes`] found and [`delete_app_volumes`] deletes, rather
    /// than those of mxv's older compose project.
    app: bool,
}

impl WorkspaceVolumes {
    /// Returns the volumes' docker names, in name order.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.volumes
            .iter()
            .map(|volume| volume.name().as_str().to_owned())
            .collect()
    }
}

/// The app that the scope of a workspace's app volumes names. The volumes
/// of a workspace's apps are shared by every app, and neither their names
/// nor their labels name an app, so any app's scope finds them.
const VOLUMES_APP: &str = "volumes";

/// Returns the scope that finds the data volumes of the apps of the
/// workspace whose stable id is `id`, which `phase` keeps.
fn app_volume_scope(phase: &Phase, id: &WorkspaceId) -> Result<Scope, ContainerError> {
    let app = AppName::try_from(VOLUMES_APP.to_owned()).map_err(|error| {
        ContainerError::failed("volume scope", None, format!("{VOLUMES_APP}: {error}"))
    })?;
    let identity = Identity {
        id: id.clone(),
        run_record: phase.run_record(id),
    };
    Ok(Scope::app(identity, &app))
}

/// Returns the docker name of the data volume `volume` of the apps of the
/// workspace whose stable id is `id`, `phase-<hash>.<volume>`, which a
/// container app creates when it first mounts the volume.
#[must_use]
pub fn app_volume_name(phase: &Phase, id: &WorkspaceId, volume: &VolumeKey) -> Option<String> {
    app_volume_scope(phase, id)
        .ok()
        .map(|scope| scope.volume_name(volume).as_str().to_owned())
}

/// Returns the labels, as `name=value` pairs in name order, that phase
/// gives the data volume `volume` of the apps of the workspace whose stable
/// id is `id`. A volume created with exactly these labels under
/// [`app_volume_name`] is one that phase mounts as its own.
#[must_use]
pub fn app_volume_labels(phase: &Phase, id: &WorkspaceId, volume: &VolumeKey) -> Vec<String> {
    let Ok(scope) = app_volume_scope(phase, id) else {
        return Vec::new();
    };
    match scope.labels(VOLUME_LABEL, volume.as_str()) {
        serde_json::Value::Object(labels) => labels
            .into_iter()
            .map(|(label, value)| format!("{label}={}", value.as_str().unwrap_or_default()))
            .collect(),
        _ => Vec::new(),
    }
}

/// Returns the data volumes of the apps of the workspace whose stable id
/// is `id`: each volume that docker labels with the id as a volume of the
/// workspace, under a name of the workspace's apps, in name order. phase's
/// `down` and `sweep` leave these volumes, and a removal of the workspace
/// deletes them with [`delete_app_volumes`].
///
/// # Errors
///
/// Returns the errors of `docker version` and [`ContainerError::Failed`].
pub fn app_volumes(phase: &Phase, id: &WorkspaceId) -> Result<WorkspaceVolumes, ContainerError> {
    let scope = app_volume_scope(phase, id)?;
    let filter = format!("label={WORKSPACE_ID_LABEL}={id}");
    let listed = quick(
        [
            "volume",
            "ls",
            "--quiet",
            "--filter",
            filter.as_str(),
            "--filter",
            &format!("label={VOLUME_LABEL}"),
        ],
        "volume ls",
    )?
    .checked("volume ls", None)?;
    let mut volumes = BTreeMap::new();
    for line in listed.lines() {
        let Ok(name) = VolumeName::new(line.trim()) else {
            continue;
        };
        match inspect_volume(&scope, &name) {
            Ok(Some(volume)) => {
                volumes.insert(name, Volume::Named(volume));
            }
            Ok(None) | Err(ContainerError::Foreign { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(WorkspaceVolumes {
        scope: (!volumes.is_empty()).then_some(scope),
        volumes: volumes.into_values().collect(),
        app: true,
    })
}

/// Deletes `volumes`, which [`app_volumes`] returned, after it checked each
/// one again. A volume that no longer exists counts as deleted.
///
/// # Errors
///
/// Returns [`ContainerError::Foreign`] for volumes that [`Lifecycle::volumes`]
/// returned and for an anonymous volume, and the errors of `docker version` and
/// [`ContainerError::Failed`].
pub fn delete_app_volumes(volumes: WorkspaceVolumes) -> Result<(), ContainerError> {
    let Some(scope) = volumes.scope.as_ref().filter(|_| volumes.app) else {
        return match volumes.volumes.first() {
            Some(first) => Err(ContainerError::foreign("volume", first.name().as_str())),
            None => Ok(()),
        };
    };
    // `app_volumes` returns named volumes only. An anonymous volume belongs
    // to its container, which removes it, so this refuses one whatever
    // built the set.
    if let Some(anonymous) = volumes
        .volumes
        .iter()
        .find(|volume| matches!(volume, Volume::Anonymous(_)))
    {
        return Err(ContainerError::foreign("volume", anonymous.name().as_str()));
    }
    // A volume-owner helper that an interrupted `up` left behind keeps
    // its volume in use, and Docker refuses to delete a volume in use.
    if let Some(workspace) = scope.workspace_id() {
        volume_owner::remove_helpers(workspace, None)?;
    }
    for volume in &volumes.volumes {
        if let Some(volume) = reinspect_volume(scope, volume)? {
            delete_volume(volume)?;
        }
    }
    Ok(())
}

/// Reports whether docker has a volume named `name`, whoever owns it.
///
/// # Errors
///
/// Returns the errors of `docker version` and [`ContainerError::Failed`].
pub fn volume_exists(name: &str) -> Result<bool, ContainerError> {
    let finished = quick(
        ["volume", "inspect", "--format", "{{.Name}}", "--", name],
        "volume inspect",
    )?;
    if finished.missing() {
        return Ok(false);
    }
    finished.checked("volume inspect", None).map(|_| true)
}

/// Returns the resident memory of each running container of `ids`, by
/// id, as `docker stats --no-stream` measures it. A container that is not
/// running, or whose memory docker does not report, is left out. It runs
/// no docker command when `ids` is empty.
///
/// # Errors
///
/// Returns the errors of `docker version` and [`ContainerError::Failed`].
pub fn container_memory(ids: &[String]) -> Result<BTreeMap<String, u64>, ContainerError> {
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut args = vec![
        "stats",
        "--no-stream",
        "--no-trunc",
        "--format",
        "{{.ID}}\t{{.MemUsage}}",
    ];
    args.extend(ids.iter().map(String::as_str));
    let listed = quick(args, "stats")?.checked("stats", None)?;
    Ok(listed.lines().filter_map(stats_line).collect())
}

/// Returns the container id and the resident bytes of one line of
/// `docker stats --format '{{.ID}}\t{{.MemUsage}}'`, such as
/// `0123abcd\t12.5MiB / 7.6GiB`, or `None` when docker reports no usage.
pub(crate) fn stats_line(line: &str) -> Option<(String, u64)> {
    let (id, usage) = line.split_once('\t')?;
    let used = usage.split('/').next()?.trim();
    let split = used.find(|char: char| !(char.is_ascii_digit() || char == '.'))?;
    let (number, unit) = used.split_at(split);
    let scale: u64 = match unit.trim() {
        "B" => 1,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        "TiB" => 1 << 40,
        "kB" | "KB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        _ => return None,
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    let whole: u64 = whole.parse().ok()?;
    // Docker prints at most a few decimal places, so the fraction is read
    // as a whole number of its own and scaled, without floating point.
    let digits = u32::try_from(fraction.len()).ok()?;
    let fraction: u64 = if fraction.is_empty() {
        0
    } else {
        fraction.parse().ok()?
    };
    let bytes = whole
        .checked_mul(scale)?
        .checked_add(fraction.checked_mul(scale)? / 10_u64.checked_pow(digits)?)?;
    Some((id.trim().to_owned(), bytes))
}

/// The containers of one workspace and their run records, kept through the
/// host `H`.
#[derive(Debug)]
pub struct Lifecycle<'h, H> {
    /// The host program's files, locks, port check and followers.
    host: &'h H,
    /// The runs directory, which holds the store id.
    runs: PathBuf,
    /// The workspace's run directory.
    dir: RunDir,
    /// The workspace.
    workspace: WorkspaceName,
    /// How long `docker stop` waits after SIGTERM.
    grace: Duration,
}

impl<'h, H: Host> Lifecycle<'h, H> {
    /// Returns the containers of the workspace `workspace` in the runs
    /// directory `runs`, whose files, locks and followers `host` keeps. A
    /// container stops with `grace` between SIGTERM and SIGKILL.
    #[must_use]
    pub fn new(host: &'h H, runs: &Path, workspace: &WorkspaceName, grace: Duration) -> Self {
        Self {
            host,
            runs: runs.to_path_buf(),
            dir: RunDir::new(runs, workspace),
            workspace: workspace.clone(),
            grace,
        }
    }

    /// Returns the path of a file of `service` with `extension`.
    fn file(&self, service: &ServiceName, extension: &str) -> PathBuf {
        self.dir.file(service.as_str(), extension)
    }

    /// Returns the store id, or `None` before the first container of any
    /// workspace started.
    fn store_id(&self) -> Result<Option<StoreId>, H::Error> {
        let record: Option<StoreIdRecord> = self.host.read_json(&self.runs.join(STORE_ID_FILE))?;
        Ok(record.map(|record| record.id))
    }

    /// Returns the workspace's stable id once its host has saved it in the
    /// workspace's record, which [`Host::saved_workspace_id`] does.
    ///
    /// # Errors
    ///
    /// Returns the host's errors of reading and writing the record.
    pub fn saved_id(&self) -> Result<SavedWorkspaceId, H::Error> {
        let id = self.host.saved_workspace_id(&self.workspace)?;
        Ok(SavedWorkspaceId { id })
    }

    /// Returns the scope of the workspace's containers, or `None` before
    /// the store's first container started. It runs no docker command.
    ///
    /// # Errors
    ///
    /// Returns the host's errors for a store id file that cannot be read.
    pub fn existing(&self) -> Result<Option<Prepared>, H::Error> {
        Ok(self.store_id()?.map(|id| Prepared {
            dir: self.dir.clone(),
            scope: Scope::new(&id, &self.workspace),
        }))
    }

    /// Returns the docker name of the named volume `volume` of the
    /// workspace's older compose project, `mxv-<id8>-<workspace>.<volume>`,
    /// when docker has it with the project's labels, or `None`.
    ///
    /// # Errors
    ///
    /// Returns [`ContainerError::Failed`] for a `prepared` of another
    /// workspace, and the errors of `docker version` and
    /// [`ContainerError::Failed`].
    pub fn legacy_volume(
        &self,
        prepared: &Prepared,
        volume: &VolumeKey,
    ) -> Result<Option<String>, H::Error> {
        let scope = self.scope_of(prepared)?;
        let name = scope.volume_name(volume);
        match inspect_volume(scope, &name) {
            Ok(Some(_)) => Ok(Some(name.as_str().to_owned())),
            Ok(None) | Err(ContainerError::Foreign { .. }) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Returns the scope of `prepared`, which this lifecycle's
    /// [`Lifecycle::existing`] returned.
    ///
    /// # Errors
    ///
    /// Returns [`ContainerError::Failed`] for a `prepared` of another
    /// workspace or runs directory, before any docker command runs.
    fn scope_of<'a>(&self, prepared: &'a Prepared) -> Result<&'a Scope, H::Error> {
        if prepared.dir == self.dir {
            Ok(&prepared.scope)
        } else {
            Err(ContainerError::unexpected(
                "scope check",
                None,
                format!(
                    "the docker scope of the run directory {} cannot act on the workspace {}",
                    prepared.dir.path().display(),
                    self.workspace
                ),
            )
            .into())
        }
    }

    /// Returns the services of the workspace that have a container record,
    /// which an older mxv wrote when it started one and `stop` deletes when
    /// it removes it. It runs no docker command.
    ///
    /// # Errors
    ///
    /// Returns [`ContainerError::Unreadable`] when the run directory cannot be
    /// read.
    pub fn recorded(&self) -> Result<BTreeSet<ServiceName>, H::Error> {
        let dir = self.dir.path();
        let unreadable = |source| ContainerError::Unreadable {
            path: dir.to_path_buf(),
            source,
        };
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
            Err(error) => return Err(unreadable(error).into()),
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
    fn recorded_id(&self, service: &ServiceName) -> Result<Option<ContainerId>, H::Error> {
        let record: Option<ContainerRecord> = self
            .host
            .read_json(&self.file(service, CONTAINER_EXTENSION))?;
        Ok(record.and_then(|record| ContainerId::new(&record.id).ok()))
    }

    /// Returns what docker reports about the recorded container of
    /// `service` in `scope`, or `None` when it has no record or docker no
    /// longer has it.
    fn inspect_recorded(
        &self,
        scope: &Scope,
        service: &ServiceName,
    ) -> Result<Option<Inspected>, H::Error> {
        match self.recorded_id(service)? {
            Some(id) => Ok(inspect_id(scope, service, &id)?),
            None => Ok(None),
        }
    }

    /// Returns the state of the recorded container of `service`, or `None`
    /// when it has no record or docker no longer has it.
    ///
    /// # Errors
    ///
    /// Returns the errors of reading the records, [`ContainerError::Foreign`],
    /// the errors of `docker version` and [`ContainerError::Failed`].
    pub fn state(
        &self,
        prepared: &Prepared,
        service: &ServiceName,
    ) -> Result<Option<ContainerState>, H::Error> {
        Ok(self
            .inspect_recorded(self.scope_of(prepared)?, service)?
            .map(|inspected| inspected.state))
    }

    /// Returns the state of the recorded container of `service`, as
    /// [`Lifecycle::state`] does. When the container runs and its log
    /// follower has ended, such as after a restart outside mxv, it starts
    /// a new follower, unless another command holds the workspace's lock.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Lifecycle::state`] and of starting the
    /// follower.
    pub fn status(
        &self,
        prepared: &Prepared,
        service: &ServiceName,
    ) -> Result<Option<ContainerState>, H::Error> {
        let Some(inspected) = self.inspect_recorded(self.scope_of(prepared)?, service)? else {
            return Ok(None);
        };
        if inspected.state.is_up()
            && !self.follower_alive(service)?
            && let Ok(_lock) = self.host.lock(&self.dir.lock(), Duration::ZERO)
        {
            self.follow(&inspected, true)?;
        }
        Ok(Some(inspected.state))
    }

    /// Adds the anonymous volumes `anonymous`, which an inspection of one
    /// of the scope's containers found, to the record of the workspace's
    /// volumes.
    fn record_anonymous(&self, anonymous: &[AnonymousVolume]) -> Result<(), H::Error> {
        self.record(anonymous.iter().map(|volume| volume.name().as_str()))
    }

    /// Adds `names` to the record of the workspace's volumes.
    fn record<'a>(&self, names: impl Iterator<Item = &'a str>) -> Result<(), H::Error> {
        let path = self.dir.path().join(VOLUMES_FILE);
        let mut record: VolumesRecord = self.host.read_json(&path)?.unwrap_or_default();
        let before = record.volumes.len();
        record.volumes.extend(names.map(str::to_owned));
        if record.volumes.len() == before {
            return Ok(());
        }
        record.version = RUN_VERSION;
        self.host.write_json(&path, &record)
    }

    /// Stops and removes the container `inspected`, and records its
    /// anonymous volumes, which stay until `mxv remove --delete-volumes`.
    fn discard(&self, inspected: Inspected) -> Result<(), H::Error> {
        stop(&inspected.container, self.grace)?;
        remove(inspected.container, AnonymousVolumes::Keep)?;
        self.record_anonymous(&inspected.anonymous)
    }

    /// Stops and removes the recorded container of `service` in `scope`,
    /// records its anonymous volumes, and deletes its record.
    fn remove_recorded(&self, scope: &Scope, service: &ServiceName) -> Result<(), H::Error> {
        if let Some(inspected) = self.inspect_recorded(scope, service)? {
            self.discard(inspected)?;
        }
        self.forget(service)
    }

    /// Stops and removes the recorded container of `service`, records its
    /// anonymous volumes, which stay until `mxv remove --delete-volumes`,
    /// and deletes its record.
    ///
    /// # Errors
    ///
    /// Returns [`ContainerError::Foreign`], the errors of
    /// `docker version`, [`ContainerError::Failed`], and the errors
    /// of reading and writing the run records.
    pub fn stop(&self, prepared: &Prepared, service: &ServiceName) -> Result<(), H::Error> {
        self.remove_recorded(self.scope_of(prepared)?, service)
    }

    /// Deletes the record and the follower record of the container of
    /// `service`.
    fn forget(&self, service: &ServiceName) -> Result<(), H::Error> {
        self.host
            .remove_file(&self.file(service, CONTAINER_EXTENSION))?;
        self.host
            .remove_file(&self.file(service, FOLLOWER_EXTENSION))
    }

    /// Stops and removes every recorded container of the workspace, for
    /// `forget`. A workspace without container records runs no docker
    /// command.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Lifecycle::stop`].
    pub fn stop_all(&self, prepared: &Prepared) -> Result<(), H::Error> {
        let scope = self.scope_of(prepared)?;
        for service in self.recorded()? {
            self.remove_recorded(scope, &service)?;
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
    pub fn writing(&self, prepared: &Prepared, service: &ServiceName) -> Result<bool, H::Error> {
        if self.follower_alive(service)? {
            return Ok(true);
        }
        let Some(inspected) = self.inspect_recorded(self.scope_of(prepared)?, service)? else {
            return Ok(false);
        };
        if !inspected.state.is_up() {
            return Ok(false);
        }
        if let Ok(_lock) = self.host.lock(&self.dir.lock(), Duration::ZERO) {
            self.follow(&inspected, true)?;
        }
        Ok(true)
    }

    /// Reports whether the log follower of `service` runs.
    fn follower_alive(&self, service: &ServiceName) -> Result<bool, H::Error> {
        self.host
            .follower_alive(&self.file(service, FOLLOWER_EXTENSION))
    }

    /// Starts a detached `docker logs --follow` that appends the output of
    /// the container `inspected` to its log until the container stops, and
    /// records the follower's process id in `<service>.follower`. With
    /// `since` the follower starts at the container's last start, which
    /// skips the lines that an earlier follower copied.
    fn follow(&self, inspected: &Inspected, since: bool) -> Result<(), H::Error> {
        let service = inspected.container.service();
        let exit_file = self.file(service, "logs-exit");
        self.host.remove_file(&exit_file)?;
        // The host starts the follower in a process group of its own, and
        // it ends when the container stops, so the engine does not wait
        // for it. A follower whose record cannot be written is stopped, so
        // the host never leaves a follower that it does not know of.
        let argv = follow_args(
            &inspected.container,
            since.then_some(inspected.started.as_str()),
        );
        self.host.follow(&Follow {
            name: service.as_str(),
            argv: &argv,
            cwd: self.dir.path(),
            log: &self.dir.log(service.as_str()),
            exit_file: &exit_file,
            record: &self.file(service, FOLLOWER_EXTENSION),
        })
    }

    /// Returns the volumes of the workspace that a removal deletes: the
    /// recorded volumes that docker still has and that are the scope's,
    /// and the anonymous volumes of its recorded containers. A recorded
    /// volume that is not the scope's is left out, so a removal never
    /// deletes it. It runs no docker command for a workspace without
    /// records of containers or volumes.
    ///
    /// # Errors
    ///
    /// Returns [`ContainerError::Missing`], [`ContainerError::Unavailable`] and
    /// [`ContainerError::Failed`].
    pub fn volumes(&self, prepared: &Prepared) -> Result<WorkspaceVolumes, H::Error> {
        let scope = self.scope_of(prepared)?;
        let record: Option<VolumesRecord> =
            self.host.read_json(&self.dir.path().join(VOLUMES_FILE))?;
        let services = self.recorded()?;
        if record.is_none() && services.is_empty() {
            return Ok(WorkspaceVolumes::default());
        }
        let prefix = scope.prefix();
        let mut volumes: BTreeMap<VolumeName, Volume> = BTreeMap::new();
        for name in record.map(|record| record.volumes).unwrap_or_default() {
            let Ok(name) = VolumeName::new(&name) else {
                continue;
            };
            let found = if name.as_str().starts_with(&prefix) {
                inspect_volume(scope, &name).map(|volume| volume.map(Volume::Named))
            } else {
                inspect_anonymous(&name).map(|volume| volume.map(Volume::Anonymous))
            };
            match found {
                Ok(Some(volume)) => {
                    volumes.insert(name, volume);
                }
                Ok(None) | Err(ContainerError::Foreign { .. }) => {}
                Err(error) => return Err(error.into()),
            }
        }
        for service in services {
            if let Some(inspected) = self.inspect_recorded(scope, &service)? {
                for volume in inspected.anonymous {
                    volumes.insert(volume.name().clone(), Volume::Anonymous(volume));
                }
            }
        }
        Ok(WorkspaceVolumes {
            scope: Some(scope.clone()),
            volumes: volumes.into_values().collect(),
            app: false,
        })
    }

    /// Deletes `volumes`, which [`Lifecycle::volumes`] returned for this
    /// workspace, after it checked each one again. A volume that no longer
    /// exists counts as deleted.
    ///
    /// # Errors
    ///
    /// Returns [`ContainerError::Foreign`] for the first of `volumes` when
    /// they belong to another scope, before any docker command runs, and
    /// for a volume whose labels changed. Returns the errors of
    /// the scope check, [`ContainerError::Missing`],
    /// [`ContainerError::Unavailable`] and [`ContainerError::Failed`].
    pub fn delete_volumes(
        &self,
        prepared: &Prepared,
        volumes: WorkspaceVolumes,
    ) -> Result<(), H::Error> {
        if volumes.app {
            return Err(ContainerError::foreign(
                "volume",
                volumes
                    .volumes
                    .first()
                    .map_or("", |volume| volume.name().as_str()),
            )
            .into());
        }
        let scope = self.scope_of(prepared)?;
        if let Some(first) = volumes.volumes.first()
            && volumes.scope.as_ref() != Some(scope)
        {
            return Err(ContainerError::foreign("volume", first.name().as_str()).into());
        }
        for volume in volumes.volumes {
            if let Some(volume) = reinspect_volume(scope, &volume)? {
                delete_volume(volume)?;
            }
        }
        Ok(())
    }
}
