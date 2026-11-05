//! The run record `phase/run/v3`: what one workspace's run planned and
//! started, and the state of each object. It names processes by pid, start
//! time and process group, and Docker objects by id or name. It holds no
//! environment value.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::name::{AppName, PortName, ProcessName, WorkspaceId};
use crate::plan::restart::{Restart, Stop};
use crate::record::{BadHex, ContainerId, DockerName, hex_id};

/// The version of the run record. phase reads every version and writes
/// only the newest, [`RUN_VERSION`]. Each version adds fields that an
/// older record lacks and that read as their defaults, so a later version
/// adds a variant here and needs no other read path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum RunSchema {
    /// `phase/run/v1`, whose host entries hold neither how the process
    /// ended nor its restart policy. It reads as v3 with neither.
    #[serde(rename = "phase/run/v1")]
    V1,
    /// `phase/run/v2`, which adds `ended` and `restart` to host entries. It
    /// reads as v3 with no readiness recorded.
    #[serde(rename = "phase/run/v2")]
    V2,
    /// `phase/run/v3`, which adds `ready` to host process and container
    /// entries. A phase that reads only v2 refuses it as written by a newer
    /// phase.
    #[serde(rename = "phase/run/v3")]
    V3,
}

impl RunSchema {
    /// The version that phase writes.
    pub(crate) const CURRENT: Self = Self::V3;
}

/// The newest version of the run record that this phase reads, and the one
/// it writes.
///
/// A keeper that an older phase started reads the record before each
/// write. Once a newer phase rewrites the record in this version, that
/// keeper refuses it as written by a newer phase, notes each refused write
/// in its diagnostic log, and records nothing more, until phase starts its
/// process again with a keeper of its own.
pub(crate) const RUN_VERSION: u32 = 3;

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
    /// Each app that a run planned, in the order phase starts apps, with
    /// the apps it depends on, so `down` stops them in the reverse order. A
    /// record that lacks it reads as empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) apps: Vec<AppEntry>,
}

/// An app of a run, and the apps it depends on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AppEntry {
    /// The app.
    pub(crate) name: AppName,
    /// The apps that it depends on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) depends: Vec<AppName>,
}

/// The port that a host process listens on, by its app port's name and the
/// host port that the plan gave it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Listen {
    /// The app port's name.
    pub(crate) name: PortName,
    /// The host port.
    pub(crate) port: u16,
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
    /// How many times the keeper has restarted the process since phase
    /// started it, which `status` shows. It is 0 for a keeper, and a
    /// record that lacks it reads as 0.
    #[serde(default)]
    pub(crate) restarts: u32,
    /// How phase stops the process, so `down` needs no plan. It is `None`
    /// for a keeper, and a process that lacks it stops with the default
    /// [`Stop`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) stop: Option<Stop>,
    /// The port that the process listens on, or `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) listen: Option<Listen>,
    /// The hash of the plan that phase last started the process from. It
    /// is `None` for a keeper and for a process that phase has not started
    /// yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) plan_hash: Option<PlanHash>,
    /// How the process last ended, or `None` when it has not ended since
    /// phase last started it. A keeper's restart keeps it, so `status`
    /// shows the last crash of a process that restarts, and phase's own
    /// start clears it. It is `None` for a keeper, and a record that lacks
    /// it reads as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ended: Option<Ended>,
    /// The restart policy that phase last started the process with, so
    /// `status` tells whether the keeper restarts a process that ended. It
    /// is `None` for a keeper, for a process that phase has not started
    /// yet, and in a `phase/run/v1` record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) restart: Option<Restart>,
    /// When phase last observed the process ready since its last start,
    /// or `None` when it has not. phase's own start and a keeper's restart
    /// clear it. A keeper records a restarted process that has no
    /// readiness probe ready once it outlasts its start grace. It is
    /// `None` for a keeper, and a record that lacks it, such as a
    /// `phase/run/v2` record, reads as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ready: Option<Ready>,
}

/// When phase observed a process ready: its readiness probe passed, or a
/// process without a probe was running. It holds no environment value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ready {
    /// When phase observed it, in milliseconds since the Unix epoch.
    pub(crate) at_ms: u64,
    /// How many times the process had been restarted when phase observed
    /// it: by its keeper for a host process, and by Docker for a
    /// container. `status` counts the readiness only while the count is
    /// the same, so a restart that the record did not hear of makes the
    /// process count as not ready.
    pub(crate) restarts: u32,
}

impl Ready {
    /// Returns the readiness that phase observed at `at_ms` after
    /// `restarts` restarts.
    pub(crate) const fn new(at_ms: u64, restarts: u32) -> Self {
        Self { at_ms, restarts }
    }

    /// Returns the readiness that phase observes now, after `restarts`
    /// restarts.
    pub(crate) fn now(restarts: u32) -> Self {
        let at_ms = crate::clock::wall_now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            });
        Self::new(at_ms, restarts)
    }
}

impl HostEntry {
    /// Reports whether the entry may name a running process: phase has not
    /// checked that it is gone.
    pub(crate) fn live(&self) -> bool {
        self.state != EntryState::Stopped
    }
}

/// How a host process ended: its exit, and whether a stop request caused
/// the end.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ended {
    /// The process's exit status or the signal that ended it, or `None`
    /// when phase could not read it. A stop that phase began itself
    /// records `None`, because the keeper that reads the status leaves the
    /// record to that operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) exit: Option<Exit>,
    /// Whether a stop request caused the end: `down`, `restart`, `sweep`,
    /// or a `TERM` or `INT` that the keeper received.
    pub(crate) requested: bool,
}

impl Ended {
    /// Returns the end of a process that a stop request ended, with
    /// `exit`.
    pub(crate) const fn requested(exit: Option<Exit>) -> Self {
        Self {
            exit,
            requested: true,
        }
    }

    /// Returns the end of a process that ended by itself, with `exit`.
    pub(crate) const fn by_itself(exit: Option<Exit>) -> Self {
        Self {
            exit,
            requested: false,
        }
    }
}

/// How a process exited. In JSON it is `{"code": 3}` or `{"signal": 9}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum Exit {
    /// The process exited with this status.
    Code(i32),
    /// This signal ended the process.
    Signal(i32),
}

impl Exit {
    /// Returns how a process with `status` exited, or `None` when the
    /// status holds neither a code nor a signal.
    pub(crate) fn of(status: std::process::ExitStatus) -> Option<Self> {
        use std::os::unix::process::ExitStatusExt as _;
        status
            .code()
            .map(Self::Code)
            .or_else(|| status.signal().map(Self::Signal))
    }

    /// Returns the exit status, or `None` when a signal ended the process.
    pub(crate) const fn code(self) -> Option<i32> {
        match self {
            Self::Code(code) => Some(code),
            Self::Signal(_) => None,
        }
    }

    /// Returns the signal that ended the process, or `None` when it exited.
    pub(crate) const fn signal(self) -> Option<i32> {
        match self {
            Self::Code(_) => None,
            Self::Signal(signal) => Some(signal),
        }
    }
}

/// What phase writes into a host process's entry before it launches the
/// process: the plan it starts from, how the process stops, and its port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Launching {
    /// The hash of the plan.
    pub(crate) plan_hash: PlanHash,
    /// How the process stops.
    pub(crate) stop: Stop,
    /// The port that the process listens on, or `None`.
    pub(crate) listen: Option<Listen>,
    /// The restart policy that its keeper follows.
    pub(crate) restart: Restart,
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
    /// The hash of the container's part of the plan that phase started it
    /// from, so a later `up` replaces a container whose part changed. It
    /// is `None` before the container starts, and a record that lacks it
    /// reads as `None`, which no plan matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) plan_hash: Option<PlanHash>,
    /// When phase last observed the container ready since it started it,
    /// or `None` when it has not. A record that lacks it, such as a
    /// `phase/run/v2` record, reads as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ready: Option<Ready>,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Started {
    /// A keeper or host process.
    Host(HostProcess),
    /// A container, by its id, with the hash of its part of the plan.
    Container(ContainerId, PlanHash),
    /// A network, by its id.
    Network(NetworkId),
    /// A volume, which its name identifies.
    Volume,
}

/// The record has no object with the key, or `Started` describes another
/// kind of object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Unplanned(pub(crate) ObjectKey);

impl RunRecord {
    /// Returns the record of a run of `workspace` from the plan whose hash
    /// is `plan_hash`, with no objects.
    pub(crate) const fn new(workspace: WorkspaceId, plan_hash: PlanHash) -> Self {
        Self {
            schema: RunSchema::CURRENT,
            workspace,
            plan_hash,
            keepers: Vec::new(),
            processes: Vec::new(),
            containers: Vec::new(),
            networks: Vec::new(),
            volumes: Vec::new(),
            apps: Vec::new(),
        }
    }

    /// Adds the keeper of `process` of `app`, and the process, as planned.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn host_process(self, app: &AppName, process: &ProcessName) -> Self {
        self.planned_host_process(app, process, None, None)
    }

    /// Adds the keeper of `process` of `app`, and the process, which stops
    /// as `stop` says and listens on `listen`, as planned.
    #[must_use]
    pub(crate) fn planned_host_process(
        mut self,
        app: &AppName,
        process: &ProcessName,
        stop: Option<Stop>,
        listen: Option<Listen>,
    ) -> Self {
        let entry = |stop, listen| HostEntry {
            app: app.clone(),
            process: process.clone(),
            state: EntryState::Planned,
            host: None,
            restarts: 0,
            stop,
            listen,
            plan_hash: None,
            ended: None,
            restart: None,
            ready: None,
        };
        self.keepers.push(entry(None, None));
        self.processes.push(entry(stop, listen));
        self
    }

    /// Adds the app `name`, which depends on `depends`.
    #[must_use]
    pub(crate) fn app(mut self, name: &AppName, depends: &[AppName]) -> Self {
        self.apps.push(AppEntry {
            name: name.clone(),
            depends: depends.to_vec(),
        });
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
            plan_hash: None,
            ready: None,
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
    /// planned, and takes the plan hash of `planned` as the plan of the
    /// latest operation. An object that this record already names keeps
    /// its state, its stop, its port and the plan it started from, which
    /// change only when phase starts it again, and an object that
    /// `planned` does not name stays, because it may still be running.
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
        for app in planned.apps {
            match self.apps.iter_mut().find(|known| known.name == app.name) {
                Some(known) => known.depends = app.depends,
                None => self.apps.push(app),
            }
        }
    }

    /// Appends each entry of `planned` that no entry of `list` matches.
    fn add_missing<T>(list: &mut Vec<T>, planned: Vec<T>, same: impl Fn(&T, &T) -> bool) {
        for entry in planned {
            if !list.iter().any(|known| same(known, &entry)) {
                list.push(entry);
            }
        }
    }

    /// Returns the keeper and the process of each host process that the
    /// record names, either of which may be missing, in the record's order
    /// of keepers and then of the processes that no keeper names.
    pub(crate) fn host_pairs(&self) -> Vec<(Option<&HostEntry>, Option<&HostEntry>)> {
        let same = |a: &HostEntry, b: &HostEntry| a.app == b.app && a.process == b.process;
        let mut pairs: Vec<_> = self
            .keepers
            .iter()
            .map(|keeper| {
                let process = self.processes.iter().find(|process| same(keeper, process));
                (Some(keeper), process)
            })
            .collect();
        pairs.extend(
            self.processes
                .iter()
                .filter(|process| !self.keepers.iter().any(|keeper| same(keeper, process)))
                .map(|process| (None, Some(process))),
        );
        pairs
    }

    /// Returns the keeper and the entry of `process` of `app`, either of
    /// which may be missing.
    pub(crate) fn host_pair(
        &self,
        app: &AppName,
        process: &ProcessName,
    ) -> (Option<&HostEntry>, Option<&HostEntry>) {
        let same = |entry: &&HostEntry| &entry.app == app && &entry.process == process;
        (
            self.keepers.iter().find(same),
            self.processes.iter().find(same),
        )
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
                entry.restarts = 0;
                entry.ended = None;
                entry.ready = None;
            }
            (ObjectKey::Container(app, process), Started::Container(id, plan_hash)) => {
                let entry = self
                    .containers
                    .iter_mut()
                    .find(|entry| &entry.app == app && &entry.process == process)
                    .ok_or_else(unplanned)?;
                entry.id = Some(id);
                entry.plan_hash = Some(plan_hash);
                entry.state = EntryState::Running;
                entry.ready = None;
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

    /// Records that the keeper restarted the host process `key` as `host`,
    /// after `restarts` restarts in all. Nothing has observed the restarted
    /// process ready yet, so it clears the readiness.
    ///
    /// # Errors
    ///
    /// Returns [`Unplanned`] when the record does not name `key` as a host
    /// process.
    pub(crate) fn mark_restarted(
        &mut self,
        key: &ObjectKey,
        host: HostProcess,
        restarts: u32,
    ) -> Result<(), Unplanned> {
        let ObjectKey::Process(app, process) = key else {
            return Err(Unplanned(key.clone()));
        };
        let entry = find_host_mut(&mut self.processes, app, process)
            .ok_or_else(|| Unplanned(key.clone()))?;
        entry.host = Some(host);
        entry.state = EntryState::Running;
        entry.restarts = restarts;
        entry.ready = None;
        Ok(())
    }

    /// Records that phase observed the host process or container `key`
    /// ready, as `ready` says.
    ///
    /// # Errors
    ///
    /// Returns [`Unplanned`] when the record does not name `key` as a host
    /// process or a container.
    pub(crate) fn mark_ready(&mut self, key: &ObjectKey, ready: Ready) -> Result<(), Unplanned> {
        let slot = match key {
            ObjectKey::Process(app, process) => {
                find_host_mut(&mut self.processes, app, process).map(|entry| &mut entry.ready)
            }
            ObjectKey::Container(app, process) => self
                .containers
                .iter_mut()
                .find(|entry| &entry.app == app && &entry.process == process)
                .map(|entry| &mut entry.ready),
            ObjectKey::Keeper(..) | ObjectKey::Network(_) | ObjectKey::Volume(_) => None,
        };
        *slot.ok_or_else(|| Unplanned(key.clone()))? = Some(ready);
        Ok(())
    }

    /// Writes the keeper and the entry of `process` of `app` back to
    /// planned with no process, before phase launches the process again,
    /// and gives the process's entry what `launching` holds.
    ///
    /// # Errors
    ///
    /// Returns [`Unplanned`] when the record names neither entry.
    pub(crate) fn mark_launching(
        &mut self,
        app: &AppName,
        process: &ProcessName,
        launching: &Launching,
    ) -> Result<(), Unplanned> {
        let keeper = find_host_mut(&mut self.keepers, app, process)
            .ok_or_else(|| Unplanned(ObjectKey::Keeper(app.clone(), process.clone())))?;
        keeper.state = EntryState::Planned;
        keeper.host = None;
        keeper.restarts = 0;
        let entry = find_host_mut(&mut self.processes, app, process)
            .ok_or_else(|| Unplanned(ObjectKey::Process(app.clone(), process.clone())))?;
        entry.state = EntryState::Planned;
        entry.host = None;
        entry.restarts = 0;
        entry.ended = None;
        entry.ready = None;
        entry.restart = Some(launching.restart.clone());
        entry.stop = Some(launching.stop);
        entry.listen.clone_from(&launching.listen);
        entry.plan_hash = Some(launching.plan_hash.clone());
        Ok(())
    }

    /// Writes the container entry of `process` of `app` back to planned
    /// with no id, before phase starts its container again, so a record
    /// that names a stopped or replaced container never stands for the new
    /// one, and `sweep` finds the new one by its name.
    ///
    /// # Errors
    ///
    /// Returns [`Unplanned`] when the record does not name the entry.
    pub(crate) fn mark_container_launching(
        &mut self,
        app: &AppName,
        process: &ProcessName,
    ) -> Result<(), Unplanned> {
        let entry = self
            .containers
            .iter_mut()
            .find(|entry| &entry.app == app && &entry.process == process)
            .ok_or_else(|| Unplanned(ObjectKey::Container(app.clone(), process.clone())))?;
        entry.state = EntryState::Planned;
        entry.id = None;
        entry.plan_hash = None;
        entry.ready = None;
        Ok(())
    }

    /// Removes the keeper and the entry of each host process of `apps`
    /// that is planned and names no process, and the containers of `apps`
    /// that are planned and have no id, because phase never tried to start
    /// them. An app with no entry left is removed too.
    pub(crate) fn forget_unstarted(&mut self, apps: &[AppName]) {
        let unstarted = |entry: &HostEntry| {
            apps.contains(&entry.app) && entry.state == EntryState::Planned && entry.host.is_none()
        };
        let both: Vec<(AppName, ProcessName)> = self
            .host_pairs()
            .into_iter()
            .filter(|(keeper, process)| {
                keeper.is_none_or(unstarted) && process.is_none_or(unstarted)
            })
            .filter_map(|(keeper, process)| {
                keeper
                    .or(process)
                    .map(|entry| (entry.app.clone(), entry.process.clone()))
            })
            .collect();
        let named = |entry: &HostEntry| {
            both.iter()
                .any(|(app, process)| &entry.app == app && &entry.process == process)
        };
        self.keepers.retain(|entry| !named(entry));
        self.processes.retain(|entry| !named(entry));
        self.containers.retain(|entry| {
            !(apps.contains(&entry.app) && entry.state == EntryState::Planned && entry.id.is_none())
        });
        let (keepers, processes, containers) = (&self.keepers, &self.processes, &self.containers);
        self.apps.retain(|app| {
            !apps.contains(&app.name)
                || keepers.iter().any(|entry| entry.app == app.name)
                || processes.iter().any(|entry| entry.app == app.name)
                || containers.iter().any(|entry| entry.app == app.name)
        });
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

    /// Records that phase begins to stop the host process `key`, so a stop
    /// request causes its end: it records the end as requested with no
    /// exit. An entry that is already `stopping` or `stopped` with an end
    /// that no stop request caused keeps that end, because the process had
    /// ended by itself before phase stopped it, as after a failed start.
    /// The end of an earlier crash that a running entry keeps is replaced.
    ///
    /// # Errors
    ///
    /// Returns [`Unplanned`] when the record does not name `key` as a host
    /// process.
    pub(crate) fn mark_stop_requested(&mut self, key: &ObjectKey) -> Result<(), Unplanned> {
        let ObjectKey::Process(app, process) = key else {
            return Err(Unplanned(key.clone()));
        };
        let entry = find_host_mut(&mut self.processes, app, process)
            .ok_or_else(|| Unplanned(key.clone()))?;
        let ended_by_itself = entry.ended.is_some_and(|ended| !ended.requested)
            && matches!(entry.state, EntryState::Stopping | EntryState::Stopped);
        if !ended_by_itself {
            entry.ended = Some(Ended::requested(None));
        }
        Ok(())
    }

    /// Reports whether a host process's entry records an end that no stop
    /// request caused, which `status` shows as exited.
    pub(crate) fn holds_an_exit(&self) -> bool {
        self.processes
            .iter()
            .any(|entry| entry.ended.is_some_and(|ended| !ended.requested))
    }

    /// Reports whether a host process that `wanted` does not select holds
    /// an end that no stop request caused. `wanted` maps each selected app
    /// to its selected processes, or to `None` for all of them. A `down` or
    /// a sweep that leaves everything stopped removes the record, unless the
    /// `down` did not select a process that holds such an end, so `status`
    /// still shows that process as exited.
    pub(crate) fn holds_an_exit_outside(
        &self,
        wanted: &BTreeMap<&AppName, Option<BTreeSet<&ProcessName>>>,
    ) -> bool {
        self.processes.iter().any(|entry| {
            entry.ended.is_some_and(|ended| !ended.requested)
                && match wanted.get(&entry.app) {
                    None => true,
                    Some(None) => false,
                    Some(Some(only)) => !only.contains(&entry.process),
                }
        })
    }

    /// Records `ended` as how the host process `key` last ended.
    ///
    /// # Errors
    ///
    /// Returns [`Unplanned`] when the record does not name `key` as a host
    /// process.
    pub(crate) fn set_ended(&mut self, key: &ObjectKey, ended: Ended) -> Result<(), Unplanned> {
        let ObjectKey::Process(app, process) = key else {
            return Err(Unplanned(key.clone()));
        };
        let entry = find_host_mut(&mut self.processes, app, process)
            .ok_or_else(|| Unplanned(key.clone()))?;
        entry.ended = Some(ended);
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
