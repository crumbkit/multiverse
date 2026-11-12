//! The units of a workspace's app: the adapter between a manifest and the
//! phase plan that starts them, stops them, reports their state and reads
//! their logs.
//!
//! phase keeps each workspace's run record and logs under its own state
//! directory, `<state>/phase`. mxv's run directory `<runs>/<workspace>/`
//! holds the lock that `up` and `down` take, the mark that a reclaim
//! stopped the workspace, and the file that says when an mxv command last
//! named it. `mxv remove` deletes the whole directory.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use phase::Phase;

use crate::archive::PortChange;
use crate::error::Error;
use crate::manifest::{Manifest, Process};
use crate::name::WorkspaceName;
use crate::phase_plan::CallerEnvironment;
use crate::ports::{self, DEFAULT_PORT_RANGE};
use crate::record::Workspace;
use crate::store::{self, LOCK_WAIT, Lock, Store};

mod phased;
#[cfg(test)]
mod tests;

pub(crate) use phased::Forgotten;

pub use phased::{
    Admission, Asked, BuildEnd, BuildStop, BuildsPassed, Exec, ExecEnd, ExecTarget, Granting,
    LogOutput, MiseListing, MiseTaskName, PermissionAsk, Place, PreparedTask, TaskEnd,
    TaskServices, TaskStart, Volumes,
};

/// How long `mxv down` waits after SIGTERM before it sends SIGKILL.
pub const STOP_GRACE: Duration = Duration::from_secs(10);

/// Why mxv does not know the exit code of a process that exited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExitReason {
    /// phase recorded no end for the process. Its keeper ended before it
    /// recorded how the process ended, as when something killed the keeper
    /// or the machine restarted.
    Unrecorded,
}

impl ExitReason {
    /// Returns the reason as a sentence that a row prints.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Unrecorded => {
                "the process ended while phase was not watching it, so nothing recorded its exit code"
            }
        }
    }
}

/// The state of one process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessState {
    /// The process or container runs.
    Running {
        /// The process id of a process, which is also its process group's
        /// id, or `None` for a container.
        pid: Option<u32>,
    },
    /// The process ended on its own, or something other than `mxv down`
    /// stopped it.
    Exited {
        /// Its exit code, or `None` when a signal ended a process. A
        /// container's code is the one docker reports.
        code: Option<i32>,
        /// The signal that ended a process, when the shell recorded one. The shell
        /// records a program that a signal ended as code 128 plus the
        /// signal's number, so a code from 129 to 192 reads as that signal.
        signal: Option<i32>,
        /// Why mxv does not know the exit code, or `None` when it knows it
        /// or a signal ended the process.
        reason: Option<ExitReason>,
    },
    /// docker created the container and has not started it, such as when
    /// `docker compose up` stopped between creating and starting it.
    Created,
    /// The process has not started since the last `mxv down`, or never.
    Stopped,
    /// phase started the process and it has not passed its readiness check
    /// yet, or its keeper runs and has not started it yet.
    Starting {
        /// The process id of a process, or `None` for a container or a
        /// process that its keeper has not started.
        pid: Option<u32>,
    },
    /// phase sent the stop signal and the process has not exited yet.
    Stopping {
        /// The process id of a process, or `None` for a container.
        pid: Option<u32>,
    },
    /// docker is restarting the container under its restart policy.
    Restarting,
    /// phase cannot tell what the unit is doing, such as when docker
    /// does not answer, and leaves it alone.
    Unknown {
        /// Why phase cannot tell, in a sentence.
        reason: &'static str,
    },
}

impl ProcessState {
    /// Reports whether a process in this state may run, which includes a
    /// process whose state phase cannot tell.
    #[must_use]
    pub const fn may_run(self) -> bool {
        !matches!(self, Self::Stopped | Self::Exited { .. } | Self::Created)
    }
}

/// Whether a name of the manifest is a process or a container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A process that mxv starts on the host.
    Process,
    /// A container that mxv runs with docker.
    Container,
}

/// The host processes and containers of a workspace that may run, as
/// [`Runs::footprint`] finds them, whose memory a reclaim measures.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Footprint {
    /// The process id of each host process, which is also its process
    /// group's id.
    pub pids: Vec<u32>,
    /// The docker id of each container.
    pub containers: Vec<String>,
}

/// One process or container of a workspace and its state.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProcessStatus {
    /// The process's or container's name.
    pub name: String,
    /// Whether it is a process or a container.
    pub kind: Kind,
    /// Its state.
    pub state: ProcessState,
    /// The host ports it receives, by variable name.
    pub ports: BTreeMap<String, u16>,
    /// Its log file, or `None` for a container that phase runs, whose log
    /// comes from `docker logs`.
    pub log: Option<PathBuf>,
    /// Whether phase has seen the process ready since it last started:
    /// `Some(true)` once its readiness check passed, or once a process
    /// without one outlasted its start grace, and `Some(false)` while it
    /// runs and phase has not seen it ready, such as after its keeper or
    /// docker restarted it. It is `None` when it does not run.
    pub ready: Option<bool>,
    /// Whether phase runs a host process under its sandbox, as phase
    /// describes it, such as `unsandboxed (its app declares no
    /// permissions)`, or `None` for a container and for a process that
    /// phase does not run.
    pub sandbox: Option<String>,
    /// What `up` and `restart` found about the process beside its state,
    /// such as secret files that `up` rewrote or kept. Every other command
    /// leaves it empty.
    pub notes: Vec<ProcessNote>,
}

/// Something that `up` or `restart` found about a process beside its
/// state.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProcessNote {
    /// `up` rewrote the secret files of the running container from a fresh
    /// environment, and the container kept running.
    SecretsRewritten {
        /// phase's sentence, which names the app and the process, such as
        /// `rewrote 2 secret files of db/main`.
        message: String,
    },
    /// `up` could not rewrite the secret files of the running container,
    /// so it still runs with its earlier files. The next `up` tries again.
    SecretsKept {
        /// phase's sentence, which names the app, the process and why, and
        /// holds no value.
        message: String,
    },
    /// `up` restarted a running host process because its part of the plan
    /// changed since phase started it, such as its `permissions` table, or
    /// because its run record does not say which part of the plan it
    /// started from.
    PlanChanged {
        /// phase's sentence, which names the app, the process and why, such
        /// as `restarted api/main because its part of the plan changed`.
        message: String,
    },
    /// `up` found a container running and could not check the owner of a
    /// volume it mounts, so it left the volume as it was. The next `up`
    /// checks again.
    VolumeOwnerUnchecked {
        /// phase's sentence, which names the app and why.
        message: String,
    },
}

impl ProcessNote {
    /// Returns phase's sentence.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::SecretsRewritten { message }
            | Self::SecretsKept { message }
            | Self::PlanChanged { message }
            | Self::VolumeOwnerUnchecked { message } => message,
        }
    }

    /// Reports whether the note is a warning, which says that something
    /// did not happen as asked.
    #[must_use]
    pub const fn is_warning(&self) -> bool {
        matches!(
            self,
            Self::SecretsKept { .. } | Self::VolumeOwnerUnchecked { .. }
        )
    }
}

/// A volume of a container of a workspace's one checkout that holds data
/// under its name without a prefix, which adding a second checkout would
/// leave behind, because the container then mounts the volume under its
/// prefixed name. [`Runs::renamed_volumes`] returns them.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RenamedVolume {
    /// The volume's name in the checkout's `mxv.toml`, such as `data`.
    pub volume: String,
    /// The docker volume that holds the data now.
    pub old: String,
    /// The docker volume that phase mounts once the workspace has several
    /// checkouts.
    pub new: String,
    /// The labels, as `name=value`, that the new volume needs, so that a
    /// volume created with them is the one phase mounts.
    pub labels: Vec<String>,
}

/// The outcome of [`Runs::up`].
#[derive(Debug)]
pub enum UpOutcome {
    /// Every selected process runs. It holds the selected processes in the
    /// order they started.
    Up(Vec<ProcessStatus>),
    /// The manifest declares no process with the name.
    UnknownProcess(String),
    /// Something outside the workspace holds a port that a process
    /// receives, so the process did not start.
    PortTaken {
        /// The process.
        process: String,
        /// The port's name.
        port: String,
        /// The host port.
        number: u16,
        /// The processes of the workspace whose process groups still have
        /// members and may hold the port, such as children that an exited
        /// process left behind. Empty when no group of the workspace lives,
        /// so a program outside the workspace holds the port.
        holders: Vec<String>,
    },
    /// A process exited before it was ready, or within the grace of a
    /// process without a readiness check.
    Exited {
        /// The process.
        process: String,
        /// Its exit code, or `None` when a signal ended it.
        code: Option<i32>,
        /// The signal that ended it, or `None`.
        signal: Option<i32>,
        /// Its log file, or `None` for a container that phase runs, whose
        /// log comes from `docker logs`.
        log: Option<PathBuf>,
    },
    /// The readiness port of a process accepted a connection, and no member
    /// of the process's group listens on it, so another program answered
    /// and `up` stopped the process.
    ForeignListener {
        /// The process.
        process: String,
        /// The port's name.
        port: String,
        /// The host port.
        number: u16,
        /// The process that holds the port, or `None` when it is one whose
        /// descriptors mxv may not read.
        pid: Option<u32>,
        /// The process's log file, or `None` for a container that phase
        /// runs.
        log: Option<PathBuf>,
    },
    /// The listen port answered, and the process that holds it descends
    /// from the process's shell but left the process group, as a daemon
    /// that starts a session of its own does. `mxv down` could not stop
    /// it, so `up` stopped it with the process.
    DetachedListener {
        /// The process.
        process: String,
        /// The port's name.
        port: String,
        /// The host port.
        number: u16,
        /// The descendant that holds the port, which `up` sent SIGKILL.
        pid: u32,
        /// The process's log file, or `None` for a container that phase
        /// runs.
        log: Option<PathBuf>,
    },
    /// A process ran past its readiness timeout before its readiness check
    /// passed, so `up` stopped it.
    NotReady {
        /// The process.
        process: String,
        /// What `up` waited for.
        waited: Waited,
        /// The timeout it missed.
        timeout: Duration,
        /// Its log file, or `None` for a container that phase runs, whose
        /// log comes from `docker logs`.
        log: Option<PathBuf>,
    },
    /// phase cannot run the manifest.
    Unrunnable(crate::phase_plan::Unrunnable),
    /// phase found a process group of a unit that an earlier stop left,
    /// which it could not stop or could not prove its own, so it started
    /// nothing more. `up` stopped what it had started.
    Left {
        /// The unit and its group.
        unstoppable: Unstoppable,
        /// The units that `up` did not reach, in phase's order.
        not_started: Vec<String>,
    },
}

/// What a readiness check that did not pass waited for.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Waited {
    /// A connection to a port.
    Port {
        /// The readiness port's name.
        port: String,
        /// The host port.
        number: u16,
    },
    /// A log line that matches a pattern.
    Log {
        /// The pattern.
        pattern: String,
    },
}

/// The outcome of [`Runs::down`].
#[derive(Debug)]
pub enum DownOutcome {
    /// One result for each selected process, in the reverse of their start
    /// order.
    Down(Vec<DownResult>),
    /// Neither the manifest nor a run record knows a process with the
    /// name.
    UnknownProcess(String),
}

/// What [`Runs::down`] did to one selected unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownResult {
    /// The unit stopped, or was not running.
    Stopped(ProcessStatus),
    /// The unit's process group outlived SIGKILL, or `down` could not prove
    /// it its own.
    Unstoppable(Unstoppable),
    /// `down` kept the unit running, because a running unit that `down`
    /// does not stop depends on it.
    StillNeeded(StillNeeded),
}

/// Whether [`Runs::down`] keeps a named unit that a running unit outside the
/// stopped set depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Needed {
    /// Keep it, and report it as [`DownResult::StillNeeded`]. `mxv down`
    /// passes it.
    Keep,
    /// Stop it anyway, as a restart, a removal and a reclaim do.
    Stop,
}

/// A unit that `down` kept running, and the running units that need it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct StillNeeded {
    /// The unit.
    pub unit: String,
    /// The running units outside the stopped set that depend on it,
    /// directly or through other units, in name order.
    pub by: Vec<String>,
    /// Those of `by` whose state phase cannot tell, which `down` counts as
    /// running.
    pub unknown: Vec<String>,
}

/// A process whose group outlived SIGKILL and phase's kill wait, or whose
/// group `down` left alone because it could not prove the group its own.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Unstoppable {
    /// The process.
    pub process: String,
    /// Its process group's id.
    pub pid: u32,
    /// Whether `down` left the group without signalling it, because the
    /// process that led it has ended and nothing proves that the members
    /// left in the group are the ones `up` started.
    pub unverified: bool,
}

/// The directory under mxv's state directory that holds the run files of
/// each workspace's processes.
const RUNS_DIR: &str = "runs";

/// The file in a workspace's run directory whose modification time is when
/// an mxv command last named the workspace. Its name starts with `.`, so it
/// is never read as a run record.
const TOUCHED_FILE: &str = ".touched";

/// The lock file in a workspace's run directory, which [`Runs::lock`]
/// takes.
const LOCK_FILE: &str = ".lock";

/// The directory under mxv's state directory that phase keeps its own state
/// in. Phase keeps each workspace's run record at
/// `<PHASE_DIR>/runs/<workspace-id>.json`, apart from mxv's records.
const PHASE_DIR: &str = "phase";

/// The keeper that [`Runs::keep_touched`] starts, which touches a
/// workspace until it is dropped.
#[derive(Debug)]
pub struct Toucher {
    /// Dropping it tells the thread to stop.
    stop: Option<mpsc::Sender<()>>,
    /// The thread that touches the workspace.
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Toucher {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The run files of every workspace's processes, under one directory.
#[derive(Clone, Debug)]
pub struct Runs {
    /// The directory that holds one directory for each workspace.
    dir: PathBuf,
    /// The phase whose state directory is `<mxv state>/phase`.
    phase: Phase,
    /// How long [`Runs::down`] waits after SIGTERM.
    grace: Duration,
    /// The host ports that [`Runs::up`] allocates from.
    port_range: RangeInclusive<u16>,
    /// How long [`Runs::lock`] and port allocation wait for a lock that
    /// another command holds.
    lock_wait: Duration,
    /// How long [`Runs::build`] lets a command's builds run, from the
    /// moment they are admitted.
    build_wait: Duration,
    /// The environment that mxv's `main` read, which phase's keepers and
    /// host processes receive part of.
    caller: CallerEnvironment,
    /// The command that provisions a workspace's services, or `None`
    /// until [`Runs::with_provisioner`] names it.
    provisioner: Option<Vec<String>>,
}

impl Runs {
    /// Returns the run files under mxv's state directory `state`, such as
    /// `~/.local/state/mxv`, which stop processes with a grace of
    /// [`STOP_GRACE`] and allocate ports from [`DEFAULT_PORT_RANGE`]. The
    /// run files live in `<state>/runs`, and phase keeps its state in
    /// `<state>/phase`.
    #[must_use]
    pub fn new(state: &Path) -> Self {
        Self {
            dir: state.join(RUNS_DIR),
            phase: Phase::new(state.join(PHASE_DIR)),
            grace: STOP_GRACE,
            port_range: DEFAULT_PORT_RANGE,
            lock_wait: LOCK_WAIT,
            build_wait: phased::BUILD_WAIT,
            caller: CallerEnvironment::default(),
            provisioner: None,
        }
    }

    /// Returns these run files with `program`, followed by the words of
    /// `leading`, as the command of the app `mxv-provision`, which creates
    /// the resources of a workspace's services, such as mxv itself and
    /// `phase provision`. The command receives the resources as one more
    /// argument. `up` cannot start a manifest with services until the
    /// command is named.
    #[must_use]
    pub fn with_provisioner(mut self, program: &Path, leading: &[&str]) -> Self {
        let mut command = vec![program.to_string_lossy().into_owned()];
        command.extend(leading.iter().map(|word| (*word).to_owned()));
        self.provisioner = Some(command);
        self
    }

    /// Returns these run files with `wait` as how long [`Runs::build`]
    /// lets a command's builds run. Only a test shortens it.
    #[doc(hidden)]
    #[must_use]
    pub const fn with_build_wait(mut self, wait: Duration) -> Self {
        self.build_wait = wait;
        self
    }

    /// Returns these run files with `program`, followed by the words of
    /// `leading`, as the program that runs phase's keepers, such as mxv
    /// itself and `phase keep`. `up` cannot start a host process until a
    /// keeper is named.
    #[must_use]
    pub fn with_keeper(mut self, program: PathBuf, leading: Vec<OsString>) -> Self {
        self.phase = self.phase.with_keeper(program, leading);
        self
    }

    /// Returns these run files with `caller` as the environment that mxv's
    /// `main` read. phase's keepers receive its `PATH` and `HOME`, host
    /// processes receive its variables of [`PASSED_THROUGH`], and phase's
    /// `docker exec`, which writes a container's secret file, receives
    /// its `PATH`, `HOME` and `DOCKER_` variables.
    ///
    /// [`PASSED_THROUGH`]: crate::PASSED_THROUGH
    #[must_use]
    pub fn with_caller(mut self, caller: CallerEnvironment) -> Self {
        self.phase = self
            .phase
            .with_inherited(caller.inherited())
            .with_docker_environment(caller.docker_environment());
        self.caller = caller;
        self
    }

    /// Returns these run files with `protected` as the paths that phase's
    /// sandboxed apps may never read or write, such as mxv's state
    /// directory and its mirrors.
    #[must_use]
    pub fn with_protected(mut self, protected: Vec<PathBuf>) -> Self {
        self.phase = self.phase.with_protected(protected);
        self
    }

    /// Returns the phase that these run files use. It is public only so
    /// that a test can start a plan that mxv's manifest cannot write yet,
    /// such as an app whose permissions name secrets.
    #[doc(hidden)]
    #[must_use]
    pub const fn phase(&self) -> &Phase {
        &self.phase
    }

    /// Returns these run files with `wait` as how long a lock waits for
    /// another command to release it.
    #[must_use]
    pub fn with_lock_wait(mut self, wait: Duration) -> Self {
        self.lock_wait = wait;
        self.phase = self.phase.with_lock_wait(wait);
        self
    }

    /// Takes the lock of the workspace `name`, which [`Runs::up`] and
    /// [`Runs::down`] require, so two commands never start or stop one
    /// workspace's app at once. The lock is an exclusive `flock` on
    /// `<runs>/<name>/.lock`, and it is released when the returned
    /// [`Lock`] drops.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Busy`] when another command holds the lock for
    /// longer than the lock wait, which is [`LOCK_WAIT`] by default, and
    /// [`Error::Unwritable`] when the lock file cannot be opened.
    pub fn lock(&self, name: &WorkspaceName) -> Result<Lock, Error> {
        store::lock(&self.workspace_dir(name).join(LOCK_FILE), self.lock_wait)
    }

    /// Records that an mxv command named the workspace `name` now, as the
    /// modification time of the file `<runs>/<name>/.touched`.
    /// [`Runs::touched`] reads it back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unwritable`] when the file cannot be written.
    pub fn touch(&self, name: &WorkspaceName) -> Result<(), Error> {
        store::touch_file(&self.workspace_dir(name).join(TOUCHED_FILE))
    }

    /// Touches the workspace `name` now and again every `every` until the
    /// returned [`Toucher`] is dropped, for a command that runs as long as
    /// a person uses it, such as `mxv exec` or `mxv logs --follow`. A touch
    /// that cannot be written is skipped, as for [`Runs::touch`].
    #[must_use]
    pub fn keep_touched(&self, name: &WorkspaceName, every: Duration) -> Toucher {
        let _ = self.touch(name);
        let (stop, stopped) = mpsc::channel::<()>();
        let runs = self.clone();
        let name = name.clone();
        // A keeper that cannot start leaves the workspace touched once, at
        // the start of the command.
        let thread = std::thread::Builder::new()
            .name("mxv-touch".to_owned())
            .spawn(move || {
                while let Err(mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(every) {
                    let _ = runs.touch(&name);
                }
            })
            .ok();
        Toucher {
            stop: Some(stop),
            thread,
        }
    }

    /// Returns when an mxv command last named the workspace `name`, as
    /// [`Runs::touch`] recorded it, or `None` when none has since the
    /// workspace's run files were deleted.
    #[must_use]
    pub fn touched(&self, name: &WorkspaceName) -> Option<SystemTime> {
        fs::metadata(self.workspace_dir(name).join(TOUCHED_FILE))
            .and_then(|metadata| metadata.modified())
            .ok()
    }

    /// Returns these run files with `range` as the host ports that
    /// [`Runs::up`] allocates from.
    #[must_use]
    pub const fn with_port_range(mut self, range: RangeInclusive<u16>) -> Self {
        self.port_range = range;
        self
    }

    /// Returns these run files with `grace` as the wait between SIGTERM
    /// and SIGKILL.
    #[must_use]
    pub const fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    /// Returns the directory of the workspace `name`'s run files.
    #[must_use]
    pub fn workspace_dir(&self, name: &WorkspaceName) -> PathBuf {
        self.dir.join(name.as_str())
    }

    /// Allocates a host port of the port range for each port of `manifest`
    /// that `workspace` does not hold, avoiding `avoid` and the ports that
    /// other workspaces in `store` hold, drops the ports of names that
    /// `manifest` no longer declares, and writes the record when it changed
    /// one. With `held`, which says that a process phase started may still
    /// run, it drops no port, because phase does not say which ports a
    /// running process received. It holds the lock `<runs>/.ports.lock` while it reads the other
    /// records and writes this one, so two workspaces that allocate at once
    /// never take the same port.
    fn assign_ports(
        &self,
        store: &Store,
        workspace: &mut Workspace,
        manifest: &Manifest,
        avoid: &BTreeSet<u16>,
        held: bool,
    ) -> Result<(), Error> {
        // The ledger holds the manifest's ports and the port of each
        // backend of its services.
        let ledger = manifest.ledger_ports();
        // A port that the manifest no longer names stays while a process
        // that received it still runs, so the old listener keeps its port
        // until `down`.
        let mut live = BTreeSet::new();
        if held {
            live.extend(workspace.ports.values().copied());
        }
        let before = workspace.ports.len();
        workspace
            .ports
            .retain(|port, number| ledger.contains(port) || live.contains(number));
        let dropped = workspace.ports.len() != before;
        if ledger.iter().all(|port| workspace.ports.contains_key(port)) {
            return if dropped {
                store.update(workspace)
            } else {
                Ok(())
            };
        }
        let _lock = store::lock(&self.dir.join(".ports.lock"), self.lock_wait)?;
        let mut reserved: BTreeSet<u16> = store
            .list()?
            .workspaces
            .iter()
            .filter(|other| other.name != workspace.name)
            .flat_map(|other| other.ports.values().copied())
            .collect();
        reserved.extend(workspace.ports.values().copied());
        reserved.extend(avoid);
        for port in &ledger {
            if !workspace.ports.contains_key(port) {
                let number = ports::allocate(&self.port_range, &reserved)?;
                reserved.insert(number);
                workspace.ports.insert(port.clone(), number);
            }
        }
        store.update(workspace)
    }

    /// Keeps each host port of `workspace`, a workspace that a restore
    /// rebuilt, that is still free and that no other workspace in `store`
    /// holds, replaces any other from the port range, as [`Runs::up`]
    /// assigns a port, and returns what changed. It calls `write` to write
    /// the record while it holds the lock of the ports, so no other
    /// workspace takes one of them first, and returns `None` when `write`
    /// reports that the record could not be written.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Store::list`], [`Error::Busy`],
    /// [`Error::NoFreePort`] and the errors of `write`.
    pub(crate) fn restored_ports(
        &self,
        workspace: &mut Workspace,
        store: &Store,
        write: impl FnOnce(&mut Workspace, &Store) -> Result<bool, Error>,
    ) -> Result<Option<Vec<PortChange>>, Error> {
        let _lock = store::lock(&self.dir.join(".ports.lock"), self.lock_wait)?;
        let changes = self.taken_ports(workspace, store)?;
        Ok(write(workspace, store)?.then_some(changes))
    }

    /// Replaces each port of `workspace` that another workspace in `store`
    /// holds or that something else has taken, and returns what changed.
    fn taken_ports(
        &self,
        workspace: &mut Workspace,
        store: &Store,
    ) -> Result<Vec<PortChange>, Error> {
        let mut reserved: BTreeSet<u16> = store
            .list()?
            .workspaces
            .iter()
            .filter(|other| other.name != workspace.name)
            .flat_map(|other| other.ports.values().copied())
            .collect();
        let held: BTreeSet<u16> = workspace.ports.values().copied().collect();
        let mut changes = Vec::new();
        for (name, number) in &mut workspace.ports {
            if !reserved.contains(number) && !ports::is_taken(*number) {
                continue;
            }
            let mut avoid = reserved.clone();
            avoid.extend(held.iter().copied());
            let replacement = ports::allocate(&self.port_range, &avoid)?;
            reserved.insert(replacement);
            changes.push(PortChange::new(name.clone(), *number, replacement));
            *number = replacement;
        }
        Ok(changes)
    }

    /// Returns which ports a restore of `workspace` would change, and
    /// changes nothing.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Store::list`] and [`Error::NoFreePort`].
    pub fn planned_ports(
        &self,
        workspace: &Workspace,
        store: &Store,
    ) -> Result<Vec<PortChange>, Error> {
        let mut copy = workspace.clone();
        self.taken_ports(&mut copy, store)
    }

    /// Gives `workspace` new host ports for the ports `names` of
    /// `manifest`, or for every port when `names` is empty, each different
    /// from the port it held, and writes the record. The caller checks
    /// that no process of the workspace runs.
    ///
    /// # Errors
    ///
    /// Returns `Ok(Err(name))` for the first name that `manifest` does not
    /// declare, and the errors of [`Store::update`], [`Error::Busy`] and
    /// [`Error::NoFreePort`].
    pub fn reset_ports(
        &self,
        _lock: &Lock,
        store: &Store,
        workspace: &mut Workspace,
        manifest: &Manifest,
        names: &[String],
    ) -> Result<Result<(), String>, Error> {
        let ledger = manifest.ledger_ports();
        if let Some(unknown) = names.iter().find(|name| !ledger.contains(name)) {
            return Ok(Err(unknown.clone()));
        }
        let mut old = BTreeSet::new();
        for port in &ledger {
            if (names.is_empty() || names.contains(port))
                && let Some(number) = workspace.ports.remove(port)
            {
                old.insert(number);
            }
        }
        self.assign_ports(store, workspace, manifest, &old, false)?;
        Ok(Ok(()))
    }
}

/// Returns the host ports that `declared` receives in `workspace`: those
/// of its `ports` by variable name, and those that its `env` values name
/// as `port.<name>`, such as `port.api` for `{port.api}`. A port that the
/// workspace does not hold yet is left out. Only the variables of `ports`
/// are in the process's environment.
fn received(workspace: &Workspace, declared: &Process) -> BTreeMap<String, u16> {
    let placeholders = declared
        .env
        .values()
        .flat_map(|value| value.ports())
        .map(|port| (format!("port.{port}"), port));
    declared
        .ports
        .iter()
        .map(|(variable, port)| (variable.clone(), port.as_str()))
        .chain(placeholders)
        .filter_map(|(key, port)| workspace.ports.get(port).map(|number| (key, *number)))
        .collect()
}
