//! The run record `phase/run/v1`: what one workspace's run planned and
//! started, and the state of each object. It names processes by pid, start
//! time and process group, and Docker objects by id or name. It holds no
//! environment value.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::name::{AppName, ProcessName, WorkspaceId};
use crate::record::{BadHex, ContainerId, DockerName, hex_id};

/// The version of the run record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum RunSchema {
    /// `phase/run/v1`.
    #[serde(rename = "phase/run/v1")]
    V1,
}

/// The newest version of the run record that this phase reads and writes.
pub(crate) const RUN_VERSION: u32 = 1;

/// The run record of one workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunRecord {
    /// The record's version.
    pub(crate) schema: RunSchema,
    /// The workspace's stable id, which names the record's file.
    pub(crate) workspace: WorkspaceId,
    /// The hash of the plan that the run started from.
    pub(crate) plan_hash: PlanHash,
    /// The keeper of each host process.
    pub(crate) keepers: Vec<HostEntry>,
    /// Each host process.
    pub(crate) processes: Vec<HostEntry>,
    /// Each container process.
    pub(crate) containers: Vec<ContainerEntry>,
    /// Each Docker network.
    pub(crate) networks: Vec<NetworkEntry>,
    /// Each Docker volume.
    pub(crate) volumes: Vec<VolumeEntry>,
}

/// Where an object is in its life. phase writes each state before the
/// step that the next state describes, so a crash leaves a record that
/// names everything that may be running.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum EntryState {
    /// phase is about to create the object, and it may exist.
    Planned,
    /// phase created the object.
    Running,
    /// phase is about to stop or remove the object, and it may still exist.
    Stopping,
    /// phase checked that the object is gone.
    Stopped,
}

/// A keeper or a host process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HostEntry {
    /// The app.
    pub(crate) app: AppName,
    /// The process, or for a keeper, the process it keeps.
    pub(crate) process: ProcessName,
    /// Where the process is in its life.
    pub(crate) state: EntryState,
    /// The process, once it has started.
    pub(crate) host: Option<HostProcess>,
}

/// A started host process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "HostFields")]
pub(crate) struct HostProcess {
    /// The process id, which is greater than 1.
    pub(crate) pid: u32,
    /// The start time that phase read when the process started, which
    /// [`crate::record::process::start_time`] describes.
    pub(crate) started_at_us: u64,
    /// The id of the process's group, which is greater than 0.
    pub(crate) group: u32,
    /// The id of the boot in which the process started, on Linux, where
    /// the start time counts from the boot. It is `None` on macOS.
    pub(crate) boot_id: Option<String>,
}

/// The fields of a [`HostProcess`] as the record holds them, before phase
/// checks them.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostFields {
    /// The process id.
    pid: u32,
    /// The start time.
    started_at_us: u64,
    /// The group id.
    group: u32,
    /// The boot id.
    boot_id: Option<String>,
}

/// The record names pid 0 or 1, or group 0, which phase never starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BadHostProcess;

impl fmt::Display for BadHostProcess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .write_str("a recorded process has a pid greater than 1 and a group greater than 0")
    }
}

impl TryFrom<HostFields> for HostProcess {
    type Error = BadHostProcess;

    fn try_from(fields: HostFields) -> Result<Self, BadHostProcess> {
        if fields.pid <= 1 || fields.group == 0 {
            return Err(BadHostProcess);
        }
        Ok(Self {
            pid: fields.pid,
            started_at_us: fields.started_at_us,
            group: fields.group,
            boot_id: fields.boot_id,
        })
    }
}

/// A container process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContainerEntry {
    /// The app.
    pub(crate) app: AppName,
    /// The process, which is the Compose service.
    pub(crate) process: ProcessName,
    /// The name that phase gives the container, which finds it before its
    /// id is recorded.
    pub(crate) name: DockerName,
    /// Where the container is in its life.
    pub(crate) state: EntryState,
    /// The container's id, once it has started.
    pub(crate) id: Option<ContainerId>,
}

/// A Docker network.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NetworkEntry {
    /// The name that phase gives the network.
    pub(crate) name: DockerName,
    /// Where the network is in its life.
    pub(crate) state: EntryState,
    /// The network's id, once it exists.
    pub(crate) id: Option<NetworkId>,
}

/// A Docker volume, which Docker identifies by its name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VolumeEntry {
    /// The volume's name.
    pub(crate) name: DockerName,
    /// How long the volume lives.
    pub(crate) lifetime: VolumeLifetime,
    /// Where the volume is in its life.
    pub(crate) state: EntryState,
}

/// How long a volume lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum VolumeLifetime {
    /// The volume belongs to one run, and `sweep` removes it.
    Run,
    /// The volume holds data that outlives a run, such as a database's,
    /// and `sweep` leaves it. Only `reset` removes it.
    Workspace,
}

/// The immutable id of a Docker network: 64 lowercase hexadecimal digits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct NetworkId(String);

impl NetworkId {
    /// Returns the id as text.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for NetworkId {
    type Error = BadHex;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        hex_id(text).map(Self)
    }
}

impl From<NetworkId> for String {
    fn from(id: NetworkId) -> Self {
        id.0
    }
}

/// The hash of a plan: 64 lowercase hexadecimal digits of a 32-byte
/// digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct PlanHash(String);

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the engine writes its runs through the write-ahead journal when `up` lands"
    )
)]
impl PlanHash {
    /// Returns the hash whose bytes are `digest`.
    pub(crate) fn new(digest: [u8; 32]) -> Self {
        Self(digest.iter().map(|byte| format!("{byte:02x}")).collect())
    }
}

impl TryFrom<String> for PlanHash {
    type Error = BadHex;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        hex_id(text).map(Self)
    }
}

impl From<PlanHash> for String {
    fn from(hash: PlanHash) -> Self {
        hash.0
    }
}

/// One object of a run record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ObjectKey {
    /// The keeper of a host process.
    Keeper(AppName, ProcessName),
    /// A host process.
    Process(AppName, ProcessName),
    /// A container process.
    Container(AppName, ProcessName),
    /// A network.
    Network(DockerName),
    /// A volume.
    Volume(DockerName),
}

impl fmt::Display for ObjectKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Keeper(app, process) => write!(formatter, "the keeper of {app}/{process}"),
            Self::Process(app, process) => write!(formatter, "the process {app}/{process}"),
            Self::Container(app, process) => write!(formatter, "the container {app}/{process}"),
            Self::Network(name) => write!(formatter, "the network {name}"),
            Self::Volume(name) => write!(formatter, "the volume {name}"),
        }
    }
}

/// What phase learned when an object started.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the engine writes its runs through the write-ahead journal when `up` lands"
    )
)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Started {
    /// A keeper or host process.
    Host(HostProcess),
    /// A container, by its id.
    Container(ContainerId),
    /// A network, by its id.
    Network(NetworkId),
    /// A volume, which its name identifies.
    Volume,
}

/// The record has no object with the key, or `Started` describes another
/// kind of object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Unplanned(pub(crate) ObjectKey);

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the engine writes its runs through the write-ahead journal when `up` lands"
    )
)]
impl RunRecord {
    /// Returns the record of a run of `workspace` from the plan whose hash
    /// is `plan_hash`, with no objects.
    pub(crate) const fn new(workspace: WorkspaceId, plan_hash: PlanHash) -> Self {
        Self {
            schema: RunSchema::V1,
            workspace,
            plan_hash,
            keepers: Vec::new(),
            processes: Vec::new(),
            containers: Vec::new(),
            networks: Vec::new(),
            volumes: Vec::new(),
        }
    }

    /// Adds the keeper of `process` of `app`, and the process, as planned.
    #[must_use]
    pub(crate) fn host_process(mut self, app: &AppName, process: &ProcessName) -> Self {
        for list in [&mut self.keepers, &mut self.processes] {
            list.push(HostEntry {
                app: app.clone(),
                process: process.clone(),
                state: EntryState::Planned,
                host: None,
            });
        }
        self
    }

    /// Adds the container of `process` of `app`, named `name`, as planned.
    #[must_use]
    pub(crate) fn container(
        mut self,
        app: &AppName,
        process: &ProcessName,
        name: DockerName,
    ) -> Self {
        self.containers.push(ContainerEntry {
            app: app.clone(),
            process: process.clone(),
            name,
            state: EntryState::Planned,
            id: None,
        });
        self
    }

    /// Adds the network `name` as planned.
    #[must_use]
    pub(crate) fn network(mut self, name: DockerName) -> Self {
        self.networks.push(NetworkEntry {
            name,
            state: EntryState::Planned,
            id: None,
        });
        self
    }

    /// Adds the volume `name`, which lives for `lifetime`, as planned.
    #[must_use]
    pub(crate) fn volume(mut self, name: DockerName, lifetime: VolumeLifetime) -> Self {
        self.volumes.push(VolumeEntry {
            name,
            lifetime,
            state: EntryState::Planned,
        });
        self
    }

    /// Adds each object of `planned` that this record does not name, as
    /// planned, and takes the plan hash of `planned`. An object that this
    /// record already names keeps its state, and an object that `planned`
    /// does not name stays, because it may still be running.
    pub(crate) fn merge(&mut self, planned: Self) {
        self.plan_hash = planned.plan_hash;
        let host = |a: &HostEntry, b: &HostEntry| a.app == b.app && a.process == b.process;
        Self::add_missing(&mut self.keepers, planned.keepers, host);
        Self::add_missing(&mut self.processes, planned.processes, host);
        Self::add_missing(&mut self.containers, planned.containers, |a, b| {
            a.app == b.app && a.process == b.process
        });
        Self::add_missing(&mut self.networks, planned.networks, |a, b| {
            a.name == b.name
        });
        Self::add_missing(&mut self.volumes, planned.volumes, |a, b| a.name == b.name);
    }

    /// Appends each entry of `planned` that no entry of `list` matches.
    fn add_missing<T>(list: &mut Vec<T>, planned: Vec<T>, same: impl Fn(&T, &T) -> bool) {
        for entry in planned {
            if !list.iter().any(|known| same(known, &entry)) {
                list.push(entry);
            }
        }
    }

    /// Returns the state of the object `key`, or `None` when the record
    /// does not name it.
    pub(crate) fn state(&self, key: &ObjectKey) -> Option<EntryState> {
        match key {
            ObjectKey::Keeper(app, process) | ObjectKey::Process(app, process) => {
                let list = if matches!(key, ObjectKey::Keeper(..)) {
                    &self.keepers
                } else {
                    &self.processes
                };
                list.iter()
                    .find(|entry| &entry.app == app && &entry.process == process)
                    .map(|entry| entry.state)
            }
            ObjectKey::Container(app, process) => self
                .containers
                .iter()
                .find(|entry| &entry.app == app && &entry.process == process)
                .map(|entry| entry.state),
            ObjectKey::Network(name) => self
                .networks
                .iter()
                .find(|entry| &entry.name == name)
                .map(|entry| entry.state),
            ObjectKey::Volume(name) => self
                .volumes
                .iter()
                .find(|entry| &entry.name == name)
                .map(|entry| entry.state),
        }
    }

    /// Records that the object `key` started, as `started` describes.
    ///
    /// # Errors
    ///
    /// Returns [`Unplanned`] when the record does not name `key`, or
    /// `started` describes another kind of object.
    pub(crate) fn mark_started(
        &mut self,
        key: &ObjectKey,
        started: Started,
    ) -> Result<(), Unplanned> {
        let unplanned = || Unplanned(key.clone());
        match (key, started) {
            (ObjectKey::Keeper(app, process), Started::Host(host)) => {
                let entry = find_host_mut(&mut self.keepers, app, process).ok_or_else(unplanned)?;
                entry.host = Some(host);
                entry.state = EntryState::Running;
            }
            (ObjectKey::Process(app, process), Started::Host(host)) => {
                let entry =
                    find_host_mut(&mut self.processes, app, process).ok_or_else(unplanned)?;
                entry.host = Some(host);
                entry.state = EntryState::Running;
            }
            (ObjectKey::Container(app, process), Started::Container(id)) => {
                let entry = self
                    .containers
                    .iter_mut()
                    .find(|entry| &entry.app == app && &entry.process == process)
                    .ok_or_else(unplanned)?;
                entry.id = Some(id);
                entry.state = EntryState::Running;
            }
            (ObjectKey::Network(name), Started::Network(id)) => {
                let entry = self
                    .networks
                    .iter_mut()
                    .find(|entry| &entry.name == name)
                    .ok_or_else(unplanned)?;
                entry.id = Some(id);
                entry.state = EntryState::Running;
            }
            (ObjectKey::Volume(name), Started::Volume) => {
                let entry = self
                    .volumes
                    .iter_mut()
                    .find(|entry| &entry.name == name)
                    .ok_or_else(unplanned)?;
                entry.state = EntryState::Running;
            }
            _ => return Err(unplanned()),
        }
        Ok(())
    }

    /// Sets the state of the object `key` to `state`.
    ///
    /// # Errors
    ///
    /// Returns [`Unplanned`] when the record does not name `key`.
    pub(crate) fn set_state(
        &mut self,
        key: &ObjectKey,
        state: EntryState,
    ) -> Result<(), Unplanned> {
        let slot = match key {
            ObjectKey::Keeper(app, process) => {
                find_host_mut(&mut self.keepers, app, process).map(|entry| &mut entry.state)
            }
            ObjectKey::Process(app, process) => {
                find_host_mut(&mut self.processes, app, process).map(|entry| &mut entry.state)
            }
            ObjectKey::Container(app, process) => self
                .containers
                .iter_mut()
                .find(|entry| &entry.app == app && &entry.process == process)
                .map(|entry| &mut entry.state),
            ObjectKey::Network(name) => self
                .networks
                .iter_mut()
                .find(|entry| &entry.name == name)
                .map(|entry| &mut entry.state),
            ObjectKey::Volume(name) => self
                .volumes
                .iter_mut()
                .find(|entry| &entry.name == name)
                .map(|entry| &mut entry.state),
        };
        *slot.ok_or_else(|| Unplanned(key.clone()))? = state;
        Ok(())
    }

    /// Reports whether every object is stopped, apart from volumes that
    /// outlive the run.
    pub(crate) fn all_stopped(&self) -> bool {
        let stopped = |state: EntryState| state == EntryState::Stopped;
        self.keepers.iter().all(|entry| stopped(entry.state))
            && self.processes.iter().all(|entry| stopped(entry.state))
            && self.containers.iter().all(|entry| stopped(entry.state))
            && self.networks.iter().all(|entry| stopped(entry.state))
            && self
                .volumes
                .iter()
                .all(|entry| entry.lifetime == VolumeLifetime::Workspace || stopped(entry.state))
    }
}

/// Returns the entry of `process` of `app` in `list`, to change.
fn find_host_mut<'r>(
    list: &'r mut [HostEntry],
    app: &AppName,
    process: &ProcessName,
) -> Option<&'r mut HostEntry> {
    list.iter_mut()
        .find(|entry| &entry.app == app && &entry.process == process)
}
