//! The processes of a workspace's app: starting them from the manifest,
//! stopping them, reporting their state, and reading their logs.
//!
//! No long-running mxv watches the processes. Each process leaves files in
//! `<runs>/<workspace>/`: `<process>.json`, the run record with the process
//! id and its start time; `<process>.log`, its standard output and standard
//! error; `<process>.log.1`, the log of its previous start or the part of
//! its log that a cap moved aside; and `<process>.exit`, which the process
//! writes when it ends. [`Runs::status`] reads them: a process without a
//! run record is stopped, a process with an exit file has exited with that
//! code, and a process whose id the process table lists with the recorded
//! start time is running. A process whose id is gone, or now names a
//! process that started at another time, has exited without a code.
//! `mxv down` deletes the run record and the exit file and keeps both
//! logs; `mxv remove` deletes the whole directory.
//!
//! A container of the manifest has the record `<container>.container`,
//! which holds the id of the container that `up` started; docker holds its
//! state. phase's container engine keeps that record, describes the
//! compose project and checks the labels before it acts on a recorded id. Its
//! log is `<container>.log` too, which a detached `docker logs --follow`
//! appends to while the container runs, so the log outlives the container
//! that `mxv down` removes. `<container>.follower` records the follower's
//! process id, so `up` and `status` start a new follower when the old one
//! has ended and the container runs, such as after a restart outside mxv.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use rustix::process::Signal;

use crate::clock::{FOLLOW_INTERVAL, POLL_INTERVAL, pause};
use phase::Phase;
use phase::container::{
    self as docker, ComposeValue, ContainerSpec, ContainerSpecs, ContainerState, Lifecycle,
    OwnedContainer, Prepared, PublishedPort, ServiceName, Start, VolumeKey, WorkspaceVolumes,
};
use phase::host::LogFollower;

use crate::error::Error;
use crate::manifest::{Container, LogPattern, Manifest, Process, ReadyCheck, Unit};
use crate::name::WorkspaceName;
use crate::phase_plan::CallerEnvironment;
use crate::ports::{self, DEFAULT_PORT_RANGE};
use crate::process::{self, Holder, Launch, Launched};
use crate::record::Workspace;
use crate::redacted::Redacted;
use crate::run_files::{
    Ownership, RUN_VERSION, RunDir, RunHost, RunRecord, ownership, read_json, remove_if_present,
    write_json,
};
use crate::store::{self, LOCK_WAIT, Lock, Store};

mod phased;
#[cfg(test)]
mod tests;

pub use phased::{Exec, ExecEnd, ExecTarget, LogOutput, Volumes};

/// How long `mxv down` waits after SIGTERM before it sends SIGKILL.
pub const STOP_GRACE: Duration = Duration::from_secs(10);

/// How long `mxv down` waits after SIGKILL before it reports the process
/// as unstoppable.
pub const KILL_WAIT: Duration = Duration::from_secs(5);

/// How long `mxv up` watches a process without a readiness check before it
/// counts the process as started. A process that exits within it fails
/// `up`.
pub const START_GRACE: Duration = Duration::from_millis(500);

/// The most bytes that a process's log holds before an mxv command that
/// reads the workspace's state moves it to `<process>.log.1` and empties
/// it.
pub const MOST_LOG_BYTES: u64 = 32 * 1024 * 1024;

pub use phase::{MOST_TAIL_BYTES, tail};

/// The most bytes that one read of a followed log takes. The rest waits
/// for the next read.
pub use phase::host::MOST_FOLLOW_BYTES;

/// Why mxv does not know the exit code of a process that exited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExitReason {
    /// The shell that ran the process has ended and wrote no exit file.
    /// Something, such as SIGKILL or a restart of the machine, ended the
    /// shell before it could write the file. Members of the process group
    /// may still run.
    NoExitFile,
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
            Self::NoExitFile => {
                "the shell that ran the process ended without writing its exit code, so something such as SIGKILL ended the shell"
            }
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
    /// phase cannot tell what the process is doing, such as when docker
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
    /// Whether its process group still has a member although the process
    /// itself has exited, such as a child it started in the background,
    /// which `mxv down` stops. It is `None` while the process runs, because
    /// the process itself is then a member.
    pub group_alive: Option<bool>,
    /// Whether phase has seen the process ready since it last started:
    /// `Some(true)` once its readiness check passed, or once a process
    /// without one outlasted its start grace, and `Some(false)` while it
    /// runs and phase has not seen it ready, such as after its keeper or
    /// docker restarted it. It is `None` when it does not run, and for a
    /// process that mxv started before phase ran its apps.
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
    /// `up` left a host process running that started before its manifest
    /// gained or lost its `permissions` table, so it still runs as it
    /// started, sandboxed or not. `mxv restart` applies the change.
    SandboxStale {
        /// mxv's sentence, which names the process and the restart that
        /// applies its permissions.
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
            | Self::SandboxStale { message } => message,
        }
    }

    /// Reports whether the note is a warning, which says that something
    /// did not happen as asked.
    #[must_use]
    pub const fn is_warning(&self) -> bool {
        matches!(self, Self::SecretsKept { .. } | Self::SandboxStale { .. })
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
    /// A process or container that mxv started before phase ran its apps
    /// may still run, so `up` started nothing. `mxv down` stops it.
    LegacyRunning {
        /// The processes and containers that may still run, in name order.
        processes: Vec<String>,
    },
    /// A container mounts a volume that holds data under the name that mxv
    /// gave it before phase ran its apps, and phase's volume of that name
    /// does not exist, so `up` started nothing. phase does not take over
    /// the older volume.
    VolumeNotMoved {
        /// The container.
        container: String,
        /// The volume's name in the manifest.
        volume: String,
        /// The older volume's docker name.
        legacy: String,
        /// The docker name of phase's volume.
        phase: String,
        /// The labels, as `name=value`, that phase's volume needs, so that
        /// a volume created with them is the one phase mounts.
        labels: Vec<String>,
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
    /// One result for each selected process, in the order `down` stopped
    /// them: the stopped process, or the group that outlived SIGKILL.
    Down(Vec<Result<ProcessStatus, Unstoppable>>),
    /// Neither the manifest nor a run record knows a process with the
    /// name.
    UnknownProcess(String),
}

/// A process whose group outlived SIGKILL and [`KILL_WAIT`], or whose
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

/// How a wait for readiness ended.
enum Readiness {
    /// The port accepted a connection, or a log line matched.
    Ready,
    /// The process ended, with how the shell ended.
    Ended(process::Ended),
    /// The timeout passed.
    TimedOut,
}

/// The directory under mxv's state directory that holds the run files of
/// each workspace's processes.
const RUNS_DIR: &str = "runs";

/// The file in a workspace's run directory whose modification time is when
/// an mxv command last named the workspace. Its name starts with `.`, so it
/// is never read as a run record.
const TOUCHED_FILE: &str = ".touched";

/// The directory under mxv's state directory that phase keeps its own state
/// in. Phase keeps each workspace's run record at
/// `<PHASE_DIR>/runs/<workspace-id>.json`, apart from mxv's records.
const PHASE_DIR: &str = "phase";

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
        store::lock(&self.run_dir(name).lock(), self.lock_wait)
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

    /// Returns the run directory of the workspace `name`.
    fn run_dir(&self, name: &WorkspaceName) -> RunDir {
        RunDir::new(&self.dir, name)
    }

    /// Returns the directory of the workspace `name`'s run files.
    #[must_use]
    pub fn workspace_dir(&self, name: &WorkspaceName) -> PathBuf {
        self.run_dir(name).path().to_path_buf()
    }

    /// Returns the log file of `process` in the workspace `name`.
    #[must_use]
    pub fn log(&self, name: &WorkspaceName, process: &str) -> PathBuf {
        self.run_dir(name).log(process)
    }

    /// Returns the path of a file of `process` with `extension`.
    fn file(&self, name: &WorkspaceName, process: &str, extension: &str) -> PathBuf {
        self.run_dir(name).file(process, extension)
    }

    /// Returns the names of the processes that have a run record, in name
    /// order.
    fn recorded(&self, name: &WorkspaceName) -> Result<Vec<String>, Error> {
        let dir = self.workspace_dir(name);
        let unreadable = |source| Error::StateUnreadable {
            path: dir.clone(),
            source,
        };
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(unreadable(error)),
        };
        let mut names = Vec::new();
        for entry in entries {
            let path = entry.map_err(unreadable)?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
                && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
                && !stem.starts_with('.')
            {
                names.push(stem.to_owned());
            }
        }
        names.sort();
        Ok(names)
    }

    /// Returns the run record of `process`, or `None`.
    fn record(&self, name: &WorkspaceName, process: &str) -> Result<Option<RunRecord>, Error> {
        read_json(&self.file(name, process, "json"))
    }

    /// Returns the state of `process` from its run files.
    fn state(&self, name: &WorkspaceName, process: &str) -> Result<ProcessState, Error> {
        let Some(record) = self.record(name, process)? else {
            return Ok(ProcessState::Stopped);
        };
        let exit_file = self.file(name, process, "exit");
        if let Some(code) = process::exit_code(&exit_file) {
            return Ok(exited(code));
        }
        if process::started_at(record.pid, &record.started)? == Some(true) {
            return Ok(ProcessState::Running {
                pid: Some(record.pid),
            });
        }
        // The shell writes the exit file before it exits, so a process that
        // ended between the two reads above has its code by now.
        Ok(match process::exit_code(&exit_file) {
            Some(code) => exited(code),
            None => ProcessState::Exited {
                code: None,
                signal: None,
                reason: Some(ExitReason::NoExitFile),
            },
        })
    }

    /// Returns the processes of `workspace` and their states: the
    /// manifest's processes in start order, then any other process that a
    /// run record names, in name order. Without a manifest it reports only
    /// the processes that a run record names.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StateUnreadable`], [`Error::CorruptRecord`] or
    /// [`Error::NewerRecord`] for a run record that cannot be read, and
    /// [`Error::ProcessTable`] when `ps` fails.
    fn legacy_status(
        &self,
        workspace: &Workspace,
        manifest: Option<&Manifest>,
    ) -> Result<Vec<ProcessStatus>, Error> {
        let containers = self.containers(workspace, manifest)?;
        let names = self.known(workspace, manifest, &containers)?;
        self.cap_logs(&workspace.name, &names)?;
        names
            .iter()
            .map(|process| self.unit_status(workspace, manifest, &containers, process))
            .collect()
    }

    /// Moves each log of `names` that holds more than [`MOST_LOG_BYTES`] to
    /// `<process>.log.1` and empties it. `status`, `logs`, `up` and `down`
    /// call it, because no mxv process runs between commands to watch the
    /// logs.
    fn cap_logs(&self, name: &WorkspaceName, names: &[String]) -> Result<(), Error> {
        for process in names {
            store::cap_log(
                &self.log(name, process),
                &self.run_dir(name).older_log(process),
                MOST_LOG_BYTES,
            )?;
        }
        Ok(())
    }

    /// Returns the status of the process or container `name`.
    fn unit_status(
        &self,
        workspace: &Workspace,
        manifest: Option<&Manifest>,
        containers: &BTreeSet<String>,
        name: &str,
    ) -> Result<ProcessStatus, Error> {
        if containers.contains(name) {
            self.container_status(workspace, manifest, name)
        } else {
            self.process_status(workspace, manifest, name)
        }
    }

    /// Reports whether `process` of the workspace `name` runs, or has left
    /// a process group with members behind, so its log may still grow.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Runs::status`].
    fn legacy_writing(&self, name: &WorkspaceName, process: &str) -> Result<bool, Error> {
        if self.record(name, process)?.is_none() {
            return self.container_writing(name, process);
        }
        Ok(
            matches!(self.state(name, process)?, ProcessState::Running { .. })
                || self.group_lives(name, process)?,
        )
    }

    /// Returns the names of the processes and containers of `workspace`
    /// that run: the processes in name order, then the containers in name
    /// order.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Runs::status`].
    fn legacy_running(&self, workspace: &Workspace) -> Result<Vec<String>, Error> {
        let mut running = Vec::new();
        for process in self.recorded(&workspace.name)? {
            if matches!(
                self.state(&workspace.name, &process)?,
                ProcessState::Running { .. }
            ) || self.group_lives(&workspace.name, &process)?
            {
                running.push(process);
            }
        }
        running.extend(self.running_containers(&workspace.name)?);
        Ok(running)
    }

    /// Returns whether the process group that the run record of `process`
    /// names is the one that `up` started, as [`ownership`] decides, or
    /// `None` when no record names it.
    fn group_ownership(
        &self,
        name: &WorkspaceName,
        process: &str,
    ) -> Result<Option<Ownership>, Error> {
        match self.record(name, process)? {
            Some(record) => Ok(Some(ownership(&record, &self.file(name, process, "exit"))?)),
            None => Ok(None),
        }
    }

    /// Reports whether the process group that the run record of `process`
    /// names may still be the one that `up` started, so that mxv starts
    /// nothing in its place.
    fn group_lives(&self, name: &WorkspaceName, process: &str) -> Result<bool, Error> {
        Ok(self
            .group_ownership(name, process)?
            .is_some_and(Ownership::may_live))
    }

    /// Returns the manifest's processes and containers in start order,
    /// then the other processes that run records name and the other
    /// `containers`.
    fn known(
        &self,
        workspace: &Workspace,
        manifest: Option<&Manifest>,
        containers: &BTreeSet<String>,
    ) -> Result<Vec<String>, Error> {
        let mut names = manifest
            .and_then(|manifest| manifest.start_order(&[]).ok())
            .unwrap_or_default();
        for name in self
            .recorded(&workspace.name)?
            .into_iter()
            .chain(containers.iter().cloned())
        {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        Ok(names)
    }

    /// Returns the status of one process.
    fn process_status(
        &self,
        workspace: &Workspace,
        manifest: Option<&Manifest>,
        process: &str,
    ) -> Result<ProcessStatus, Error> {
        let ports = match manifest.and_then(|manifest| manifest.processes.get(process)) {
            Some(declared) => received(workspace, declared),
            None => self
                .record(&workspace.name, process)?
                .map(|record| record.ports)
                .unwrap_or_default(),
        };
        let state = self.state(&workspace.name, process)?;
        let group_alive = match state {
            ProcessState::Exited { .. } => Some(self.group_lives(&workspace.name, process)?),
            ProcessState::Stopped | ProcessState::Created => Some(false),
            // A run file gives only the three states above and running.
            ProcessState::Running { .. }
            | ProcessState::Starting { .. }
            | ProcessState::Stopping { .. }
            | ProcessState::Restarting
            | ProcessState::Unknown { .. } => None,
        };
        Ok(ProcessStatus {
            name: process.to_owned(),
            kind: Kind::Process,
            state,
            ports,
            log: Some(self.log(&workspace.name, process)),
            group_alive,
            ready: None,
            sandbox: None,
            notes: Vec::new(),
        })
    }

    /// Starts the processes `names` of `manifest`, or every process when
    /// `names` is empty, with their dependencies first, in the checkout at
    /// `checkout`. It skips a process that already runs. It allocates a
    /// host port for each named port that `workspace` does not hold yet,
    /// and writes the record to `store` when it does.
    ///
    /// Each process starts after its dependencies are ready. Before it
    /// starts a listener, `up` checks that the listen port is free. A
    /// process with a readiness check counts as ready when its port accepts
    /// a TCP connection, and one without counts as ready once it has run
    /// for [`START_GRACE`] without exiting. Each start moves the process's
    /// previous log to `<process>.log.1`.
    /// When a process fails, `up` stops the processes that it started
    /// before it, so either every selected process runs or this run leaves
    /// none of its own running. The failed process keeps its run record
    /// when it exited, so `mxv status` reports its exit code.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Store::list`] and [`Store::update`],
    /// [`Error::Busy`] when another command allocates ports for longer
    /// than the lock wait, [`Error::NoFreePort`] when no port can be allocated, the errors of
    /// [`Runs::status`], [`Error::ProcessUnstartable`] for a process that
    /// cannot start, and [`Error::Unwritable`] when a run file cannot be
    /// written.
    #[doc(hidden)]
    pub fn legacy_up(
        &self,
        _lock: &Lock,
        store: &Store,
        workspace: &mut Workspace,
        checkout: &Path,
        manifest: &Manifest,
        names: &[String],
    ) -> Result<UpOutcome, Error> {
        let order = match manifest.start_order(names) {
            Ok(order) => order,
            Err(unknown) => return Ok(UpOutcome::UnknownProcess(unknown)),
        };
        self.cap_logs(&workspace.name, &order)?;
        let prepared = if order
            .iter()
            .any(|name| manifest.containers.contains_key(name))
        {
            // The record on disk holds the workspace's stable id before any
            // docker object is labelled with it.
            let host = RunHost {
                records: Some(store),
            };
            let lifecycle = self.lifecycle_with(&host, &workspace.name);
            let saved = lifecycle.saved_id()?;
            workspace.id = Some(saved.id().clone());
            Some(lifecycle.prepare(&self.phase, &saved)?)
        } else {
            None
        };
        let specs = container_specs(&manifest.containers);
        self.assign_ports(store, workspace, manifest, &BTreeSet::new(), false)?;
        let mut started = Vec::new();
        for process in &order {
            let started_one = match (manifest.unit(process), &prepared) {
                (None, _) => return Ok(UpOutcome::UnknownProcess(process.clone())),
                (Some(Unit::Container(declared)), Some(prepared)) => {
                    if self.container_runs(prepared, &workspace.name, process)? {
                        // A container that runs is left as it is, apart
                        // from a log follower that it may have lost.
                        self.start_container(workspace, prepared, &specs, process, declared)?;
                        continue;
                    }
                    self.start_container(workspace, prepared, &specs, process, declared)
                }
                (Some(Unit::Container(_)), None) => {
                    return Ok(UpOutcome::UnknownProcess(process.clone()));
                }
                (Some(Unit::Process(declared)), _) => {
                    if matches!(
                        self.state(&workspace.name, process)?,
                        ProcessState::Running { .. }
                    ) {
                        continue;
                    }
                    self.start(workspace, checkout, process, declared)
                }
            };
            match started_one {
                Ok(None) => started.push(process),
                Ok(Some(outcome)) => {
                    self.roll_back(&workspace.name, manifest, prepared.as_ref(), &started);
                    return Ok(outcome);
                }
                Err(error) => {
                    self.roll_back(&workspace.name, manifest, prepared.as_ref(), &started);
                    return Err(error);
                }
            }
        }
        let containers: BTreeSet<String> = manifest.containers.keys().cloned().collect();
        let mut statuses = Vec::new();
        for process in &order {
            statuses.push(self.unit_status(workspace, Some(manifest), &containers, process)?);
        }
        Ok(UpOutcome::Up(statuses))
    }

    /// Stops the processes that a failed `up` started, in the reverse of
    /// their start order, and deletes their run records, so a failed `up`
    /// leaves no process of its own running. Each step is best effort,
    /// because the caller reports the failure that stopped `up`.
    fn roll_back(
        &self,
        name: &WorkspaceName,
        manifest: &Manifest,
        prepared: Option<&Prepared>,
        started: &[&String],
    ) {
        for process in started.iter().rev() {
            if manifest.containers.contains_key(*process) {
                // `up` prepared the containers' scope before it started
                // one.
                if let Some(prepared) = prepared
                    && let Some(service) = service(process)
                {
                    let _ = self.lifecycle(name).stop(prepared, &service);
                }
                continue;
            }
            if let Ok(Some(record)) = self.record(name, process) {
                self.stop_group(record.pid);
            }
            let _ = fs::remove_file(self.file(name, process, "json"));
            let _ = fs::remove_file(self.file(name, process, "exit"));
        }
    }

    /// Starts one process and waits for its readiness. It returns `None`
    /// when the process is ready, and the outcome that stops `up`
    /// otherwise.
    fn start(
        &self,
        workspace: &Workspace,
        checkout: &Path,
        process: &str,
        declared: &Process,
    ) -> Result<Option<UpOutcome>, Error> {
        let ports = received(workspace, declared);
        let name = &workspace.name;
        if let Some(port) = &declared.listen
            && let Some(number) = workspace.ports.get(port).copied()
            && ports::is_taken(number)
        {
            return Ok(Some(UpOutcome::PortTaken {
                process: process.to_owned(),
                port: port.clone(),
                number,
                holders: if self.group_lives(name, process)? {
                    vec![process.to_owned()]
                } else {
                    Vec::new()
                },
            }));
        }
        let exit_file = self.file(name, process, "exit");
        remove_if_present(&exit_file)?;
        let log = self.log(name, process);
        store::rotate_log(&log, &self.run_dir(name).older_log(process))?;
        // The log readiness check reads the log from its start, which after
        // the rotation holds only what this run of the process writes.
        let mut watch = match declared.ready.as_ref().map(|ready| &ready.check) {
            Some(ReadyCheck::Log(pattern)) => Some((Follower::new_log(&log)?, pattern)),
            _ => None,
        };
        let env: Vec<(String, Redacted)> = declared
            .env
            .iter()
            .map(|(variable, value)| (variable.clone(), value.render(&workspace.ports)))
            .chain(declared.ports.iter().filter_map(|(variable, port)| {
                workspace
                    .ports
                    .get(port)
                    .map(|number| (variable.clone(), Redacted::new(number.to_string())))
            }))
            .collect();
        let mut launched = process::launch(&Launch {
            name: process,
            argv: &declared.argv,
            cwd: &checkout.join(&declared.cwd),
            env: &env,
            log: &log,
            exit_file: &exit_file,
        })?;
        if let Err(error) = self.write_record(name, process, &launched, ports) {
            launched.kill();
            return Err(error);
        }
        let (number, timeout) = match &declared.ready {
            Some(ready) => (
                match &ready.check {
                    ReadyCheck::Port(port) => workspace.ports.get(port).copied(),
                    ReadyCheck::Log(_) => None,
                },
                ready.timeout,
            ),
            None => (None, START_GRACE),
        };
        let watch = watch
            .as_mut()
            .map(|(follower, pattern)| (follower, *pattern));
        // Dropping `launched` on an error stops the process, and its run
        // files go with it, so a failed `up` leaves nothing running.
        let readiness = match wait_ready(&mut launched, number, watch, timeout) {
            Ok(readiness) => readiness,
            Err(error) => {
                launched.kill();
                self.drop_run_files(name, process);
                return Err(error);
            }
        };
        match readiness {
            Readiness::Ready => self.check_listener(name, process, declared, number, launched, log),
            Readiness::Ended(shell) => {
                // The shell is reaped. Its group keeps its id only while a
                // descendant remains, so the signal goes only to a group
                // that still exists, and reaches only those descendants.
                if process::group_exists(launched.pid()) {
                    let _ = process::signal(launched.pid(), Signal::KILL);
                }
                // The shell is reaped and its group handled, so dropping the
                // handle must not signal the group's id again.
                launched.keep();
                let (code, signal) = match process::exit_code(&exit_file) {
                    Some(code) => code_and_signal(code),
                    None => (shell.code, shell.signal),
                };
                Ok(Some(UpOutcome::Exited {
                    process: process.to_owned(),
                    code,
                    signal,
                    log: Some(log),
                }))
            }
            Readiness::TimedOut => {
                let Some(ready) = &declared.ready else {
                    launched.keep();
                    return Ok(None);
                };
                launched.kill();
                let _ = fs::remove_file(self.file(name, process, "json"));
                let _ = fs::remove_file(&exit_file);
                let waited = match &ready.check {
                    ReadyCheck::Port(port) => Waited::Port {
                        port: port.clone(),
                        number: number.unwrap_or_default(),
                    },
                    ReadyCheck::Log(pattern) => Waited::Log {
                        pattern: pattern.as_str().to_owned(),
                    },
                };
                Ok(Some(UpOutcome::NotReady {
                    process: process.to_owned(),
                    waited,
                    timeout: ready.timeout,
                    log: Some(log),
                }))
            }
        }
    }

    /// Checks, for a `process` that is ready on its listen port `number`,
    /// which process holds the port. The port that answered must be the
    /// process's own, or the process would count as ready while another
    /// program serves its port. It keeps `launched` running and returns
    /// `None` when a member of the process's group holds the port, and
    /// stops it and returns the outcome that stops `up` otherwise.
    fn check_listener(
        &self,
        name: &WorkspaceName,
        process: &str,
        declared: &Process,
        number: Option<u16>,
        launched: Launched,
        log: PathBuf,
    ) -> Result<Option<UpOutcome>, Error> {
        let (Some(listen), Some(number)) = (&declared.listen, number) else {
            launched.keep();
            return Ok(None);
        };
        if declared
            .ready
            .as_ref()
            .is_none_or(|ready| ready.check != ReadyCheck::Port(listen.clone()))
        {
            launched.keep();
            return Ok(None);
        }
        let holder = match process::listen_holder(launched.pid(), number) {
            Ok(None | Some(Holder::Group)) => {
                launched.keep();
                return Ok(None);
            }
            Ok(Some(holder)) => holder,
            Err(error) => {
                launched.kill();
                self.drop_run_files(name, process);
                return Err(error);
            }
        };
        // The descendant may lead a group of its own, such as after setsid,
        // and that group goes with it.
        if let Holder::Descendant(pid) = holder {
            let _ = process::signal(pid, Signal::KILL);
            let _ = process::signal_process(pid, Signal::KILL);
        }
        launched.kill();
        self.drop_run_files(name, process);
        Ok(Some(match holder {
            Holder::Descendant(pid) => UpOutcome::DetachedListener {
                process: process.to_owned(),
                port: listen.clone(),
                number,
                pid,
                log: Some(log),
            },
            Holder::Group | Holder::Other(_) => UpOutcome::ForeignListener {
                process: process.to_owned(),
                port: listen.clone(),
                number,
                pid: match holder {
                    Holder::Other(pid) => pid,
                    Holder::Group | Holder::Descendant(_) => None,
                },
                log: Some(log),
            },
        }))
    }

    /// Deletes the run record and the exit file of `process`, which `up`
    /// stopped before it was ready.
    fn drop_run_files(&self, name: &WorkspaceName, process: &str) {
        let _ = fs::remove_file(self.file(name, process, "json"));
        let _ = fs::remove_file(self.file(name, process, "exit"));
    }

    /// Writes the run record of the process that was just launched.
    fn write_record(
        &self,
        name: &WorkspaceName,
        process: &str,
        launched: &Launched,
        ports: BTreeMap<String, u16>,
    ) -> Result<(), Error> {
        let pid = launched.pid();
        let started = process::start_time(pid)?.ok_or_else(|| Error::ProcessTable {
            message: format!("ps does not list the process {pid} that mxv just started"),
        })?;
        let record = RunRecord {
            version: RUN_VERSION,
            pid,
            started,
            ports,
        };
        write_json(&self.file(name, process, "json"), &record)
    }

    /// Stops the processes `names` of `workspace`, or every process that a
    /// run record names when `names` is empty, in the reverse of their
    /// start order. The process group that a run record names gets SIGTERM
    /// whenever it still has a member, even when the process itself has
    /// exited and left children behind, then SIGKILL when it outlives the
    /// grace. Each stopped process's run record and
    /// exit file are deleted, and its log stays. A process group that
    /// outlives SIGKILL does not stop the others, and its result says so.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Runs::status`], and [`Error::Unwritable`]
    /// when a run file cannot be deleted.
    fn legacy_down(
        &self,
        _lock: &Lock,
        workspace: &Workspace,
        manifest: Option<&Manifest>,
        names: &[String],
    ) -> Result<DownOutcome, Error> {
        let containers = self.existing_containers(&workspace.name)?;
        let known = self.known(workspace, manifest, &containers)?;
        self.cap_logs(&workspace.name, &known)?;
        let recorded = self.recorded(&workspace.name)?;
        let targets: Vec<&String> = if names.is_empty() {
            known
                .iter()
                .filter(|process| recorded.contains(process) || containers.contains(*process))
                .collect()
        } else {
            if let Some(unknown) = names.iter().find(|name| !known.contains(name)) {
                return Ok(DownOutcome::UnknownProcess(unknown.clone()));
            }
            known
                .iter()
                .filter(|process| names.contains(process))
                .collect()
        };
        let mut results = Vec::new();
        for process in targets.into_iter().rev() {
            // A record whose pid now names another process's group is
            // stale: its group is gone, so `down` drops the record and
            // signals nothing.
            let is_container = manifest
                .is_some_and(|manifest| manifest.containers.contains_key(process))
                || (!recorded.contains(process) && containers.contains(process));
            if is_container {
                if containers.contains(process)
                    && let Some(service) = service(process)
                {
                    let lifecycle = self.lifecycle(&workspace.name);
                    if let Some(prepared) = lifecycle.existing()? {
                        lifecycle.stop(&prepared, &service)?;
                    }
                }
                results.push(Ok(self.container_status_of(
                    workspace,
                    manifest,
                    process,
                    ProcessState::Stopped,
                )));
                continue;
            }
            if let Some(record) = self.record(&workspace.name, process)? {
                let exit_file = self.file(&workspace.name, process, "exit");
                let owned = ownership(&record, &exit_file)?;
                let stopped = match owned {
                    Ownership::Ours => self.stop_group(record.pid),
                    // `down` keeps the record of a group it cannot prove
                    // its own and reports the group, which it never
                    // signals.
                    Ownership::Unverified => false,
                    Ownership::Gone => true,
                };
                if !stopped {
                    results.push(Err(Unstoppable {
                        process: process.clone(),
                        pid: record.pid,
                        unverified: owned == Ownership::Unverified,
                    }));
                    continue;
                }
            }
            remove_if_present(&self.file(&workspace.name, process, "json"))?;
            remove_if_present(&self.file(&workspace.name, process, "exit"))?;
            results.push(Ok(self.process_status(workspace, manifest, process)?));
        }
        Ok(DownOutcome::Down(results))
    }

    /// Sends SIGTERM to the group `pid`, waits up to the grace, then sends
    /// SIGKILL and waits up to [`KILL_WAIT`]. It reports whether the group
    /// is gone.
    fn stop_group(&self, pid: u32) -> bool {
        let _ = process::signal(pid, Signal::TERM);
        if wait_gone(pid, self.grace) {
            return true;
        }
        let _ = process::signal(pid, Signal::KILL);
        wait_gone(pid, KILL_WAIT)
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
        for process in self.recorded(&workspace.name)? {
            if let Some(record) = self.record(&workspace.name, &process)?
                && ownership(&record, &self.file(&workspace.name, &process, "exit"))?.may_live()
            {
                live.extend(record.ports.values().copied());
            }
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

    /// Removes the stopped containers of the workspace `name`, and deletes
    /// every run file and log of it. Deleting files that do not exist
    /// succeeds. The workspace's volumes stay; [`Runs::delete_volumes`]
    /// deletes them.
    ///
    /// # Errors
    ///
    /// Returns the docker errors of [`Runs::down`], and
    /// [`Error::Unwritable`] when the directory cannot be deleted.
    fn legacy_forget(&self, name: &WorkspaceName) -> Result<(), Error> {
        let lifecycle = self.lifecycle(name);
        if !lifecycle.recorded()?.is_empty()
            && let Some(prepared) = lifecycle.existing()?
        {
            lifecycle.stop_all(&prepared)?;
        }
        let dir = self.workspace_dir(name);
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(Error::Unwritable { path: dir, source }),
        }
    }
}

/// The containers of a workspace's app, which `Lifecycle` in phase's
/// container engine starts, reports and stops. These methods put them among
/// the workspace's processes and wait for their readiness.
impl Runs {
    /// Returns the containers of the workspace `name`.
    fn lifecycle(&self, name: &WorkspaceName) -> Lifecycle<'static, RunHost<'static>> {
        self.lifecycle_with(&RunHost::WITHOUT_RECORDS, name)
    }

    /// Returns the containers of the workspace `name`, kept through `host`.
    fn lifecycle_with<'h, 's>(
        &self,
        host: &'h RunHost<'s>,
        name: &WorkspaceName,
    ) -> Lifecycle<'h, RunHost<'s>> {
        Lifecycle::new(host, &self.dir, name, self.grace, self.lock_wait)
    }

    /// Returns the names of the containers that `manifest` declares, and
    /// the containers of the workspace that the run records name.
    fn containers(
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
    fn existing_containers(&self, name: &WorkspaceName) -> Result<BTreeSet<String>, Error> {
        Ok(self
            .lifecycle(name)
            .recorded()?
            .into_iter()
            .map(|service| service.as_str().to_owned())
            .collect())
    }

    /// Returns the containers of the workspace `name` that have a record,
    /// in name order. It runs no docker command.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StateUnreadable`] when the run directory cannot be
    /// read.
    fn legacy_docker_containers(&self, name: &WorkspaceName) -> Result<Vec<String>, Error> {
        Ok(self.existing_containers(name)?.into_iter().collect())
    }

    /// Returns the containers of the workspace `name` that run, in name
    /// order.
    fn running_containers(&self, name: &WorkspaceName) -> Result<Vec<String>, Error> {
        let lifecycle = self.lifecycle(name);
        let recorded = lifecycle.recorded()?;
        if recorded.is_empty() {
            return Ok(Vec::new());
        }
        let Some(prepared) = lifecycle.existing()? else {
            return Ok(Vec::new());
        };
        let mut running = Vec::new();
        for service in recorded {
            if lifecycle
                .state(&prepared, &service)?
                .is_some_and(ContainerState::is_up)
            {
                running.push(service.as_str().to_owned());
            }
        }
        Ok(running)
    }

    /// Returns the status of the container `container`, which docker
    /// reports, as `Lifecycle::status` describes.
    fn container_status(
        &self,
        workspace: &Workspace,
        manifest: Option<&Manifest>,
        container: &str,
    ) -> Result<ProcessStatus, Error> {
        let lifecycle = self.lifecycle(&workspace.name);
        let state = match (service(container), lifecycle.existing()?) {
            (Some(service), Some(prepared)) => lifecycle.status(&prepared, &service)?,
            _ => None,
        };
        let state = state.map_or(ProcessState::Stopped, container_state);
        Ok(self.container_status_of(workspace, manifest, container, state))
    }

    /// Returns the status of the container `container` in `state`.
    fn container_status_of(
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
            log: Some(self.run_dir(&workspace.name).log(container)),
            group_alive: None,
            ready: None,
            sandbox: None,
            notes: Vec::new(),
        }
    }

    /// Reports whether the container `container` of the workspace `name`
    /// runs.
    fn container_runs(
        &self,
        prepared: &Prepared,
        name: &WorkspaceName,
        container: &str,
    ) -> Result<bool, Error> {
        match service(container) {
            Some(service) => Ok(self
                .lifecycle(name)
                .state(prepared, &service)?
                .is_some_and(ContainerState::is_up)),
            None => Ok(false),
        }
    }

    /// Reports whether the log of the container `container` may still
    /// grow, as `Lifecycle::writing` describes.
    fn container_writing(&self, name: &WorkspaceName, container: &str) -> Result<bool, Error> {
        let lifecycle = self.lifecycle(name);
        match (service(container), lifecycle.existing()?) {
            (Some(service), Some(prepared)) => lifecycle.writing(&prepared, &service),
            _ => Ok(false),
        }
    }

    /// Starts the container `name` of `specs` and waits for its
    /// readiness, as [`Runs::up`] describes, or makes sure that a container
    /// that already runs has a log follower. `declared` is the manifest's
    /// container, whose readiness check `up` waits for. It returns `None`
    /// when the container is ready, and the outcome that stops `up`
    /// otherwise. When a step after `docker compose up` fails, the
    /// container is removed before the error returns.
    fn start_container(
        &self,
        workspace: &Workspace,
        prepared: &Prepared,
        specs: &ContainerSpecs,
        name: &str,
        declared: &Container,
    ) -> Result<Option<UpOutcome>, Error> {
        let Some(service) = service(name) else {
            return Ok(Some(UpOutcome::UnknownProcess(name.to_owned())));
        };
        let lifecycle = self.lifecycle(&workspace.name);
        match lifecycle.start(prepared, specs, &service, &workspace.ports)? {
            Start::Running => Ok(None),
            Start::PortTaken { port, number } => Ok(Some(UpOutcome::PortTaken {
                process: name.to_owned(),
                port,
                number,
                holders: Vec::new(),
            })),
            Start::Gone => Ok(Some(self.exited(&workspace.name, name, None))),
            Start::Started(container) => {
                match self.wait_container(workspace, &lifecycle, prepared, declared, &container) {
                    Ok(outcome) => Ok(outcome),
                    Err(error) => {
                        let _ = lifecycle.stop(prepared, &service);
                        Err(error)
                    }
                }
            }
        }
    }

    /// Returns the outcome of the container `name` that stopped before it
    /// was ready, with docker's exit code `code`.
    fn exited(&self, name: &WorkspaceName, container: &str, code: Option<i32>) -> UpOutcome {
        let log = self.run_dir(name).log(container);
        // `docker logs` may not have copied the last lines yet.
        pause(FOLLOW_INTERVAL);
        UpOutcome::Exited {
            process: container.to_owned(),
            code,
            signal: None,
            log: Some(log),
        }
    }

    /// Waits for the readiness of `container`, which `docker compose up`
    /// just started for `declared`: its readiness port answers, or it runs
    /// past the grace of a container without a readiness check. A
    /// container that stops first, or misses its readiness timeout, ends
    /// the wait with the outcome that stops `up`, and one that misses the
    /// timeout is removed.
    fn wait_container(
        &self,
        workspace: &Workspace,
        lifecycle: &Lifecycle<'_, RunHost>,
        prepared: &Prepared,
        declared: &Container,
        container: &OwnedContainer,
    ) -> Result<Option<UpOutcome>, Error> {
        let name = container.service().as_str();
        let log = self.run_dir(&workspace.name).log(name);
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
            let state = docker::reinspect(container)?.map(|inspected| inspected.state);
            if !state.is_some_and(|state| state.is_up() || state == ContainerState::Created) {
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
                lifecycle.stop(prepared, container.service())?;
                return Ok(Some(UpOutcome::NotReady {
                    process: name.to_owned(),
                    waited: Waited::Port {
                        port: ready_port.cloned().unwrap_or_default(),
                        number: port.unwrap_or_default(),
                    },
                    timeout: ready.timeout,
                    log: Some(log),
                }));
            }
            pause(FOLLOW_INTERVAL);
        }
    }

    /// Returns the volumes of the workspace `name` that a removal deletes:
    /// the recorded volumes that docker still has and whose labels name
    /// the workspace, and the anonymous volumes of its recorded
    /// containers. It runs no docker command for a workspace without
    /// records of containers or volumes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DockerMissing`], [`Error::DockerUnavailable`] and
    /// [`Error::DockerFailed`].
    fn legacy_volumes(&self, name: &WorkspaceName) -> Result<WorkspaceVolumes, Error> {
        let lifecycle = self.lifecycle(name);
        Ok(match lifecycle.existing()? {
            Some(prepared) => lifecycle.volumes(&prepared)?,
            None => WorkspaceVolumes::default(),
        })
    }

    /// Deletes `volumes`, which [`Runs::legacy_volumes`] returned for the
    /// workspace `name`, after it checked each one again.
    fn legacy_delete_volumes(
        &self,
        name: &WorkspaceName,
        volumes: WorkspaceVolumes,
    ) -> Result<(), Error> {
        if volumes.names().is_empty() {
            return Ok(());
        }
        let lifecycle = self.lifecycle(name);
        match lifecycle.existing()? {
            Some(prepared) => lifecycle.delete_volumes(&prepared, volumes),
            None => Ok(()),
        }
    }
}

/// Returns the container engine's specs of the manifest's containers
/// `containers`. A container or volume whose name no manifest accepts is
/// left out; the manifest refuses such names when it is read.
pub(crate) fn container_specs(containers: &BTreeMap<String, Container>) -> ContainerSpecs {
    containers
        .iter()
        .filter_map(|(name, container)| {
            let mut spec = ContainerSpec::new(container.image.clone());
            spec.command.clone_from(&container.command);
            spec.env = container
                .env
                .iter()
                .map(|(variable, value)| {
                    (
                        variable.clone(),
                        ComposeValue::new(value.reveal().to_owned()),
                    )
                })
                .collect();
            spec.ports = container
                .ports
                .iter()
                .map(|published| PublishedPort::new(published.container, published.port.clone()))
                .collect();
            spec.volumes = container
                .volumes
                .iter()
                .filter_map(|(volume, path)| {
                    VolumeKey::new(volume).ok().map(|key| (key, path.clone()))
                })
                .collect();
            service(name).map(|service| (service, spec))
        })
        .collect()
}

/// Returns the service name of the container `name`, or `None` for a name
/// that no manifest container has.
fn service(name: &str) -> Option<ServiceName> {
    ServiceName::new(name).ok()
}

/// Returns the state that mxv reports for a container in `state`. The
/// exit code is docker's, which mxv does not read as a signal.
const fn container_state(state: ContainerState) -> ProcessState {
    match state {
        // mxv reports a paused container, and one that docker is
        // restarting, as running, as it did before phase told them apart.
        ContainerState::Running | ContainerState::Paused | ContainerState::Restarting => {
            ProcessState::Running { pid: None }
        }
        ContainerState::Created => ProcessState::Created,
        ContainerState::Exited(code) => ProcessState::Exited {
            code: Some(code),
            signal: None,
            reason: None,
        },
        // A status that docker printed and phase does not know, such as
        // `removing`, gives no exit code.
        ContainerState::Unknown => ProcessState::Exited {
            code: None,
            signal: None,
            reason: None,
        },
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

/// Waits until `launched` accepts a connection on `port`, writes a log
/// line that `watch`'s pattern matches, ends, or runs past `timeout`.
/// Without a port or a pattern it waits for `timeout` and returns
/// [`Readiness::TimedOut`] when the process still runs. Neither the process,
/// the port nor the log gives an event to wait on, so the wait polls them
/// every [`POLL_INTERVAL`].
fn wait_ready(
    launched: &mut Launched,
    port: Option<u16>,
    mut watch: Option<(&mut Follower, &LogPattern)>,
    timeout: Duration,
) -> Result<Readiness, Error> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some((follower, pattern)) = watch.as_mut()
            && follower.lines()?.iter().any(|line| pattern.is_match(line))
        {
            return Ok(Readiness::Ready);
        }
        if let Some(ended) = launched.ended() {
            return Ok(Readiness::Ended(ended));
        }
        if port.is_some_and(ports::accepts) {
            return Ok(Readiness::Ready);
        }
        if Instant::now() >= deadline {
            return Ok(Readiness::TimedOut);
        }
        pause(POLL_INTERVAL);
    }
}

/// Waits until the process group `pid` is gone or `timeout` passes, and
/// reports whether it is gone. The group gives no event to wait on, so the
/// wait polls it every [`POLL_INTERVAL`].
fn wait_gone(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !process::group_exists(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        pause(POLL_INTERVAL);
    }
}

/// Returns the state of a process whose shell recorded `code`, the exit
/// status of its program, which is 128 plus the signal's number when a
/// signal ended the program.
fn exited(code: Option<i32>) -> ProcessState {
    let (code, signal) = code_and_signal(code);
    ProcessState::Exited {
        code,
        signal,
        reason: None,
    }
}

/// Returns the exit code and the signal of a program whose shell recorded
/// `code`: a code from 129 to 192 is 128 plus the number of the signal that
/// ended the program.
fn code_and_signal(code: Option<i32>) -> (Option<i32>, Option<i32>) {
    match code {
        Some(code) if (129..=192).contains(&code) => (None, Some(code - 128)),
        code => (code, None),
    }
}

/// A log file that `mxv logs -f` reads as it grows, through phase's
/// [`LogFollower`].
#[derive(Debug)]
pub struct Follower {
    /// The follower of the log.
    log: LogFollower,
}

impl Follower {
    /// Creates the log at `path` when it does not exist, and opens it at
    /// its start, for a process that `up` is about to start.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unwritable`] when the log cannot be created, and
    /// [`Error::StateUnreadable`] when it cannot be opened.
    pub(crate) fn new_log(path: &Path) -> Result<Self, Error> {
        drop(store::append_file(path)?);
        let log = LogFollower::from_start(path).map_err(|source| Error::StateUnreadable {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self { log })
    }

    /// Opens the log at `path` at its end, or returns `None` when it does
    /// not exist.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StateUnreadable`] when the log cannot be opened.
    pub fn at_end(path: &Path) -> Result<Option<Self>, Error> {
        Ok(LogFollower::at_end(path)
            .map_err(|source| Error::StateUnreadable {
                path: path.to_path_buf(),
                source,
            })?
            .map(|log| Self { log }))
    }

    /// Returns the complete lines written since the last read, up to
    /// [`MOST_FOLLOW_BYTES`] bytes of them; the rest waits for the next
    /// read. A line longer than [`MOST_FOLLOW_BYTES`] comes back in parts.
    /// When a cap empties the log, the next read starts at its start.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StateUnreadable`] when the log cannot be read.
    pub fn lines(&mut self) -> Result<Vec<String>, Error> {
        self.log.lines().map_err(|source| Error::StateUnreadable {
            path: self.log.path().to_path_buf(),
            source,
        })
    }

    /// Pauses for [`FOLLOW_INTERVAL`] before the next read. A log file
    /// gives no event when it grows, so a follower polls it.
    pub fn wait(&self) {
        pause(FOLLOW_INTERVAL);
    }
}
