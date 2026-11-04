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
//! state. The `docker` module describes the compose project and the labels
//! that mxv checks before it acts on a recorded id. Its
//! log is `<container>.log` too, which a detached `docker logs --follow`
//! appends to while the container runs, so the log outlives the container
//! that `mxv down` removes. `<container>.follower` records the follower's
//! process id, so `up` and `status` start a new follower when the old one
//! has ended and the container runs, such as after a restart outside mxv.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::process::Signal;
use serde::{Deserialize, Serialize};

use crate::clock::{FOLLOW_INTERVAL, POLL_INTERVAL, pause};
use crate::docker;
use crate::error::Error;
use crate::manifest::{Manifest, Process, Unit};
use crate::name::WorkspaceName;
use crate::ports::{self, DEFAULT_PORT_RANGE};
use crate::process::{self, Launch, Launched};
use crate::record::Workspace;
use crate::store::{self, LOCK_WAIT, Lock, Store};

mod containers;
#[cfg(test)]
mod tests;

/// The version of the run record that this version of multiverse writes
/// and reads. A run record with a higher version is refused.
pub const RUN_VERSION: u32 = 1;

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

/// The number of log lines that a failed start reports.
pub const TAIL_LINES: usize = 20;

/// The most bytes from the end of a log that a tail reads. A tail of a
/// log whose last lines are longer than this returns fewer lines.
pub const MOST_TAIL_BYTES: u64 = 64 * 1024;

/// The most bytes that one read of a followed log takes. The rest waits
/// for the next read.
pub const MOST_FOLLOW_BYTES: u64 = 1024 * 1024;

/// The most bytes of a run record that multiverse reads.
const MOST_RUN_BYTES: u64 = 64 * 1024;

/// The run record of one started process.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct RunRecord {
    /// The record's version, which is [`RUN_VERSION`].
    pub(super) version: u32,
    /// The process id, which is also the id of its process group.
    pub(super) pid: u32,
    /// The process's start time as `ps` printed it, which tells the
    /// process from a later one that reuses its id.
    pub(super) started: String,
    /// The ports that the process received, by variable name.
    pub(super) ports: BTreeMap<String, u16>,
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
    },
    /// docker created the container and has not started it, such as when
    /// `docker compose up` stopped between creating and starting it.
    Created,
    /// The process has not started since the last `mxv down`, or never.
    Stopped,
}

/// Whether a name of the manifest is a process or a container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A process that mxv starts on the host.
    Process,
    /// A container that mxv runs with docker.
    Container,
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
    /// Its log file.
    pub log: PathBuf,
    /// Whether its process group still has a member although the process
    /// itself has exited, such as a child it started in the background,
    /// which `mxv down` stops. It is `None` while the process runs, because
    /// the process itself is then a member.
    pub group_alive: Option<bool>,
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
        /// Its log file.
        log: PathBuf,
        /// The last lines of its log.
        tail: Vec<String>,
    },
    /// A process ran past its readiness timeout without accepting a
    /// connection on its readiness port, so `up` stopped it.
    NotReady {
        /// The process.
        process: String,
        /// The readiness port's name.
        port: String,
        /// The host port.
        number: u16,
        /// The timeout it missed.
        timeout: Duration,
        /// Its log file.
        log: PathBuf,
        /// The last lines of its log.
        tail: Vec<String>,
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

/// A process whose group outlived SIGKILL and [`KILL_WAIT`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Unstoppable {
    /// The process.
    pub process: String,
    /// Its process group's id.
    pub pid: u32,
}

/// How a wait for readiness ended.
enum Readiness {
    /// The port accepted a connection.
    Ready,
    /// The process ended, with how the shell ended.
    Ended(process::Ended),
    /// The timeout passed.
    TimedOut,
}

/// The run files of every workspace's processes, under one directory.
#[derive(Clone, Debug)]
pub struct Runs {
    /// The directory that holds one directory for each workspace.
    dir: PathBuf,
    /// How long [`Runs::down`] waits after SIGTERM.
    grace: Duration,
    /// The host ports that [`Runs::up`] allocates from.
    port_range: RangeInclusive<u16>,
    /// How long [`Runs::lock`] and port allocation wait for a lock that
    /// another command holds.
    lock_wait: Duration,
}

impl Runs {
    /// Returns the run files under `dir`, such as `~/.local/state/mxv/runs`,
    /// which stop processes with a grace of [`STOP_GRACE`] and allocate
    /// ports from [`DEFAULT_PORT_RANGE`].
    #[must_use]
    pub const fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            grace: STOP_GRACE,
            port_range: DEFAULT_PORT_RANGE,
            lock_wait: LOCK_WAIT,
        }
    }

    /// Returns these run files with `wait` as how long a lock waits for
    /// another command to release it.
    #[must_use]
    pub const fn with_lock_wait(mut self, wait: Duration) -> Self {
        self.lock_wait = wait;
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
        store::lock(&self.workspace_dir(name).join(".lock"), self.lock_wait)
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

    /// Returns the log file of `process` in the workspace `name`.
    #[must_use]
    pub fn log(&self, name: &WorkspaceName, process: &str) -> PathBuf {
        self.workspace_dir(name).join(format!("{process}.log"))
    }

    /// Returns the path of a file of `process` with `extension`.
    fn file(&self, name: &WorkspaceName, process: &str, extension: &str) -> PathBuf {
        self.workspace_dir(name)
            .join(format!("{process}.{extension}"))
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
        if process::start_time(record.pid)?.as_deref() == Some(record.started.as_str()) {
            return Ok(ProcessState::Running {
                pid: Some(record.pid),
            });
        }
        // The shell writes the exit file before it exits, so a process that
        // ended between the two reads above has its code by now.
        Ok(exited(process::exit_code(&exit_file).flatten()))
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
    pub fn status(
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
                &self.file(name, process, "log.1"),
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
    pub fn writing(&self, name: &WorkspaceName, process: &str) -> Result<bool, Error> {
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
    pub fn running(&self, workspace: &Workspace) -> Result<Vec<String>, Error> {
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

    /// Reports whether the process group that the run record of `process`
    /// names is the one that `up` started and still has a member, as
    /// [`owns_group`] decides.
    fn group_lives(&self, name: &WorkspaceName, process: &str) -> Result<bool, Error> {
        match self.record(name, process)? {
            Some(record) => owns_group(&record),
            None => Ok(false),
        }
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
            ProcessState::Running { .. } => None,
            ProcessState::Exited { .. } => Some(self.group_lives(&workspace.name, process)?),
            ProcessState::Stopped | ProcessState::Created => Some(false),
        };
        Ok(ProcessStatus {
            name: process.to_owned(),
            kind: Kind::Process,
            state,
            ports,
            log: self.log(&workspace.name, process),
            group_alive,
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
    pub fn up(
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
        let scope = if order
            .iter()
            .any(|name| manifest.containers.contains_key(name))
        {
            docker::check()?;
            docker::check_compose()?;
            Some(self.ensure_scope(&workspace.name)?)
        } else {
            None
        };
        self.assign_ports(store, workspace, manifest, &BTreeSet::new())?;
        let mut started = Vec::new();
        for process in &order {
            let started_one = match (manifest.unit(process), &scope) {
                (None, _) => return Ok(UpOutcome::UnknownProcess(process.clone())),
                (Some(Unit::Container(declared)), Some(scope)) => {
                    if self.container_runs(&workspace.name, process)? {
                        // A container that runs is left as it is, apart
                        // from a log follower that it may have lost.
                        self.start_container(workspace, scope, manifest, process, declared)?;
                        continue;
                    }
                    self.start_container(workspace, scope, manifest, process, declared)
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
                    self.roll_back(&workspace.name, manifest, &started);
                    return Ok(outcome);
                }
                Err(error) => {
                    self.roll_back(&workspace.name, manifest, &started);
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
    fn roll_back(&self, name: &WorkspaceName, manifest: &Manifest, started: &[&String]) {
        for process in started.iter().rev() {
            if manifest.containers.contains_key(*process) {
                if let Ok(Some(scope)) = self.scope(name) {
                    let _ = self.remove_container(name, &scope, process);
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
        store::rotate_log(&log, &self.file(name, process, "log.1"))?;
        let env: Vec<(String, String)> = declared
            .env
            .iter()
            .map(|(variable, value)| (variable.clone(), value.clone()))
            .chain(
                ports
                    .iter()
                    .map(|(variable, number)| (variable.clone(), number.to_string())),
            )
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
            Some(ready) => (workspace.ports.get(&ready.port).copied(), ready.timeout),
            None => (None, START_GRACE),
        };
        match wait_ready(&mut launched, number, timeout) {
            Readiness::Ready => Ok(None),
            Readiness::Ended(shell) => {
                // The shell is reaped. Its group keeps its id only while a
                // descendant remains, so the signal goes only to a group
                // that still exists, and reaches only those descendants.
                if process::group_exists(launched.pid()) {
                    let _ = process::signal(launched.pid(), Signal::KILL);
                }
                let (code, signal) = match process::exit_code(&exit_file) {
                    Some(code) => code_and_signal(code),
                    None => (shell.code, shell.signal),
                };
                Ok(Some(UpOutcome::Exited {
                    process: process.to_owned(),
                    code,
                    signal,
                    tail: tail(&log, TAIL_LINES),
                    log,
                }))
            }
            Readiness::TimedOut => {
                let Some(ready) = &declared.ready else {
                    return Ok(None);
                };
                let number = number.unwrap_or_default();
                launched.kill();
                let _ = fs::remove_file(self.file(name, process, "json"));
                let _ = fs::remove_file(&exit_file);
                Ok(Some(UpOutcome::NotReady {
                    process: process.to_owned(),
                    port: ready.port.clone(),
                    number,
                    timeout: ready.timeout,
                    tail: tail(&log, TAIL_LINES),
                    log,
                }))
            }
        }
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
    pub fn down(
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
                    && let Some(scope) = self.scope(&workspace.name)?
                {
                    self.remove_container(&workspace.name, &scope, process)?;
                }
                results.push(Ok(self.container_status_of(
                    workspace,
                    manifest,
                    process,
                    ProcessState::Stopped,
                )));
                continue;
            }
            if let Some(record) = self.record(&workspace.name, process)?
                && owns_group(&record)?
                && !self.stop_group(record.pid)
            {
                let pid = record.pid;
                results.push(Err(Unstoppable {
                    process: process.clone(),
                    pid,
                }));
                continue;
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
    /// one. It holds the lock `<runs>/.ports.lock` while it reads the other
    /// records and writes this one, so two workspaces that allocate at once
    /// never take the same port.
    fn assign_ports(
        &self,
        store: &Store,
        workspace: &mut Workspace,
        manifest: &Manifest,
        avoid: &BTreeSet<u16>,
    ) -> Result<(), Error> {
        // A port that the manifest no longer names stays while a process
        // that received it still runs, so the old listener keeps its port
        // until `down`.
        let mut live = BTreeSet::new();
        for process in self.recorded(&workspace.name)? {
            if let Some(record) = self.record(&workspace.name, &process)?
                && owns_group(&record)?
            {
                live.extend(record.ports.values().copied());
            }
        }
        let before = workspace.ports.len();
        workspace
            .ports
            .retain(|port, number| manifest.ports.contains(port) || live.contains(number));
        let dropped = workspace.ports.len() != before;
        if manifest
            .ports
            .iter()
            .all(|port| workspace.ports.contains_key(port))
        {
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
        for port in &manifest.ports {
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
        if let Some(unknown) = names.iter().find(|name| !manifest.ports.contains(name)) {
            return Ok(Err(unknown.clone()));
        }
        let mut old = BTreeSet::new();
        for port in &manifest.ports {
            if (names.is_empty() || names.contains(port))
                && let Some(number) = workspace.ports.remove(port)
            {
                old.insert(number);
            }
        }
        self.assign_ports(store, workspace, manifest, &old)?;
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
    pub fn forget(&self, name: &WorkspaceName) -> Result<(), Error> {
        self.remove_project(name)?;
        let dir = self.workspace_dir(name);
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(Error::Unwritable { path: dir, source }),
        }
    }
}

/// Returns the host ports that `declared` receives in `workspace`, by
/// variable name. A port that the workspace does not hold yet is left out.
fn received(workspace: &Workspace, declared: &Process) -> BTreeMap<String, u16> {
    declared
        .ports
        .iter()
        .filter_map(|(variable, port)| {
            workspace
                .ports
                .get(port)
                .map(|number| (variable.clone(), *number))
        })
        .collect()
}

/// Waits until `launched` accepts a connection on `port`, ends, or runs
/// past `timeout`. Without a port it waits for `timeout` and returns
/// [`Readiness::TimedOut`] when the process still runs. Neither the process
/// nor the port gives an event to wait on, so the wait polls them every
/// [`POLL_INTERVAL`].
fn wait_ready(launched: &mut Launched, port: Option<u16>, timeout: Duration) -> Readiness {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(ended) = launched.ended() {
            return Readiness::Ended(ended);
        }
        if port.is_some_and(ports::accepts) {
            return Readiness::Ready;
        }
        if Instant::now() >= deadline {
            return Readiness::TimedOut;
        }
        pause(POLL_INTERVAL);
    }
}

/// Reports whether the process group that `record` names is the one that
/// `up` started and still has a member. It is when the process with the
/// recorded id has the recorded start time, and when no process has that
/// id but a group with it still exists: a group keeps its id while any
/// member lives, and no new process can take that id meanwhile, so the
/// group is the one `up` started even after its leader exited. A process
/// with the recorded id and another start time took a free id after the
/// group ended, so the group is not ours.
pub(super) fn owns_group(record: &RunRecord) -> Result<bool, Error> {
    Ok(match process::start_time(record.pid)? {
        Some(started) => started == record.started,
        None => process::group_exists(record.pid),
    })
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
    ProcessState::Exited { code, signal }
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

/// A run file in JSON whose version mxv checks.
pub(super) trait Versioned {
    /// The file's version, or `None` for a file without one.
    fn version(&self) -> Option<u32>;
}

impl Versioned for RunRecord {
    fn version(&self) -> Option<u32> {
        Some(self.version)
    }
}

impl Versioned for BTreeSet<String> {
    fn version(&self) -> Option<u32> {
        None
    }
}

/// Reads the run file at `path` in JSON, or returns `None` when it does
/// not exist. It reads at most [`MOST_RUN_BYTES`] bytes.
///
/// # Errors
///
/// Returns [`Error::StateUnreadable`], [`Error::CorruptRecord`] for a file
/// that does not parse, and [`Error::NewerRecord`] for a version above
/// [`RUN_VERSION`].
pub(super) fn read_json<T: for<'de> Deserialize<'de> + Versioned>(
    path: &Path,
) -> Result<Option<T>, Error> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(Error::StateUnreadable {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let mut bytes = Vec::new();
    file.take(MOST_RUN_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|source| Error::StateUnreadable {
            path: path.to_path_buf(),
            source,
        })?;
    let value: T = serde_json::from_slice(&bytes).map_err(|source| Error::CorruptRecord {
        path: path.to_path_buf(),
        source,
    })?;
    if let Some(version) = value.version()
        && version > RUN_VERSION
    {
        return Err(Error::NewerRecord {
            path: path.to_path_buf(),
            version,
        });
    }
    Ok(Some(value))
}

/// Writes `value` in JSON to the run file at `path`, replacing it whole.
///
/// # Errors
///
/// Returns [`Error::Unwritable`] when the file cannot be written.
pub(super) fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), Error> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|source| Error::CorruptRecord {
        path: path.to_path_buf(),
        source,
    })?;
    bytes.push(b'\n');
    store::replace_file(path, &bytes)
}

/// Deletes the file at `path`, and succeeds when it does not exist.
pub(super) fn remove_if_present(path: &Path) -> Result<(), Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Unwritable {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Returns the last `lines` lines of the file at `path`, read from its
/// last [`MOST_TAIL_BYTES`] bytes, or no lines when it cannot be read.
#[must_use]
pub fn tail(path: &Path, lines: usize) -> Vec<String> {
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let length = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let start = length.saturating_sub(MOST_TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file.take(MOST_TAIL_BYTES).read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut all: Vec<&str> = text.lines().collect();
    if start > 0 && !all.is_empty() {
        // The read started inside a line, so its first line is partial.
        all.remove(0);
    }
    let skip = all.len().saturating_sub(lines);
    all.into_iter().skip(skip).map(str::to_owned).collect()
}

/// A log file that `mxv logs -f` reads as it grows.
#[derive(Debug)]
pub struct Follower {
    /// The log's path, for errors.
    path: PathBuf,
    /// The open log, positioned after the last byte read.
    file: File,
    /// The bytes after the last line break, which wait for the rest of
    /// their line.
    partial: Vec<u8>,
}

impl Follower {
    /// Opens the log at `path` at its end, or returns `None` when it does
    /// not exist.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StateUnreadable`] when the log cannot be opened.
    pub fn at_end(path: &Path) -> Result<Option<Self>, Error> {
        let unreadable = |source| Error::StateUnreadable {
            path: path.to_path_buf(),
            source,
        };
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(unreadable(error)),
        };
        file.seek(SeekFrom::End(0)).map_err(unreadable)?;
        Ok(Some(Self {
            path: path.to_path_buf(),
            file,
            partial: Vec::new(),
        }))
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
        let unreadable = |source| Error::StateUnreadable {
            path: self.path.clone(),
            source,
        };
        let position = self.file.stream_position().map_err(unreadable)?;
        let length = self.file.metadata().map_err(unreadable)?.len();
        if length < position {
            // A cap emptied the log, so the next line starts at its start.
            self.file.seek(SeekFrom::Start(0)).map_err(unreadable)?;
            self.partial.clear();
        }
        let mut bytes = Vec::new();
        (&mut self.file)
            .take(MOST_FOLLOW_BYTES)
            .read_to_end(&mut bytes)
            .map_err(|source| Error::StateUnreadable {
                path: self.path.clone(),
                source,
            })?;
        self.partial.extend_from_slice(&bytes);
        let Some(end) = self.partial.iter().rposition(|byte| *byte == b'\n') else {
            if u64::try_from(self.partial.len()).unwrap_or(u64::MAX) > MOST_FOLLOW_BYTES {
                // A line longer than the bound is returned in parts, so the
                // buffer never holds more than one read's worth of bytes.
                let part = std::mem::take(&mut self.partial);
                return Ok(vec![String::from_utf8_lossy(&part).into_owned()]);
            }
            return Ok(Vec::new());
        };
        let rest = self.partial.split_off(end + 1);
        let complete = std::mem::replace(&mut self.partial, rest);
        Ok(String::from_utf8_lossy(&complete)
            .lines()
            .map(str::to_owned)
            .collect())
    }

    /// Pauses for [`FOLLOW_INTERVAL`] before the next read. A log file
    /// gives no event when it grows, so a follower polls it.
    pub fn wait(&self) {
        pause(FOLLOW_INTERVAL);
    }
}
