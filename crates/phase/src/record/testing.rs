//! Helpers for the tests of the run record and `sweep`: real processes that
//! the tests start and record, a fake process table, and a fake Docker.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::time::Duration;

use rustix::process::Signal;

use crate::engine::container::{ContainerState, Identity as WorkspaceIdentity};
use crate::engine::host::{Holder, HostRunner, HostStop, HostStopped};
use crate::engine::keeper::{Kept, Launch};
use crate::engine::probe::LogLines;
use crate::error::PhaseError;
use crate::name::{AppName, ProcessName, WorkspaceId};
use crate::record::docker::{Docker, DockerError, DockerKind, DockerObject, SeenContainer};
use crate::record::process::{self, Identity, Processes, Start, System};
use crate::record::run::{HostProcess, PlanHash, RunRecord};

/// Returns the workspace id of the tests.
pub(crate) fn workspace() -> WorkspaceId {
    "0b9f3c8e".parse().unwrap()
}

/// Returns the app name `name`.
pub(crate) fn app(name: &str) -> AppName {
    name.parse().unwrap()
}

/// Returns the process name `name`.
pub(crate) fn process(name: &str) -> ProcessName {
    name.parse().unwrap()
}

/// Returns an empty record of [`workspace`].
pub(crate) fn empty_record() -> RunRecord {
    RunRecord::new(workspace(), PlanHash::new([7; 32]))
}

/// Returns the recorded process `pid`, which leads its group and started
/// at `started_at_us` in no particular boot.
pub(crate) fn host_at(pid: u32, started_at_us: u64) -> HostProcess {
    HostProcess {
        pid,
        started_at_us,
        group: pid,
        boot_id: None,
    }
}

/// Reports whether a process has the pid `pid`.
pub(crate) fn alive(pid: u32) -> bool {
    !matches!(System.start_time(pid).unwrap(), Start::Absent)
}

/// A process that a test started in a group of its own. Dropping it kills
/// the group and reaps the process.
#[derive(Debug)]
pub(crate) struct Spawned {
    /// The child.
    #[expect(
        clippy::disallowed_types,
        reason = "the test holds the child it started and kills it when it drops"
    )]
    child: std::process::Child,
    /// The lines that the script printed before the test went on.
    pub(crate) lines: Vec<String>,
}

impl Spawned {
    /// Starts `sh -c script` in a new process group, and waits until it
    /// prints `lines` lines, the last of which it prints once it is ready.
    pub(crate) fn shell_lines(script: &str, lines: usize) -> Self {
        Self::shell_in_group(script, lines, 0)
    }

    /// Starts `sh -c script` in the process group `group`, or in a new one
    /// when `group` is 0, and waits until it prints `lines` lines.
    pub(crate) fn shell_in_group(script: &str, lines: usize, group: u32) -> Self {
        #[expect(
            clippy::disallowed_methods,
            reason = "the test starts a real process for sweep to stop"
        )]
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", script])
            .process_group(i32::try_from(group).unwrap())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[expect(
            clippy::disallowed_methods,
            reason = "the test starts a real process for sweep to stop"
        )]
        let mut child = command.spawn().unwrap();
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        let mut printed = Vec::new();
        for _ in 0..lines {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            printed.push(line.trim().to_owned());
        }
        Self {
            child,
            lines: printed,
        }
    }

    /// Starts `sh -c script`, and waits until it prints one line.
    pub(crate) fn shell(script: &str) -> Self {
        Self::shell_lines(script, 1)
    }

    /// Starts a shell that prints a line and then sleeps for 30 seconds.
    pub(crate) fn sleeper() -> Self {
        Self::shell("echo ready; exec sleep 30")
    }

    /// Returns the process's pid.
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Returns the process as phase records it.
    pub(crate) fn host(&self) -> HostProcess {
        recorded(self.pid(), self.pid())
    }

    /// Reports whether the process is still running. A process that
    /// `sweep` has reaped is no longer this test's child, and is not.
    pub(crate) fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Reports whether the process ends within two seconds, and reaps it
    /// when it does. A test asks this after phase found the process gone,
    /// because macOS stops listing a process that a signal ended a moment
    /// before its parent can reap it.
    pub(crate) fn ends_soon(&mut self) -> bool {
        let deadline = crate::clock::deadline_in(std::time::Duration::from_secs(2));
        while self.running() {
            if crate::clock::now() >= deadline {
                return false;
            }
            crate::clock::pause(crate::clock::POLL_INTERVAL);
        }
        true
    }

    /// Waits up to five seconds for the process, a member of `group`, to
    /// end, and leaves it unreaped, so that it stays in the group as a
    /// zombie.
    pub(crate) fn wait_for_end_unreaped(&self, group: u32) {
        let group = super::process::ProcessGroup::new(group).unwrap();
        let deadline = crate::clock::deadline_in(std::time::Duration::from_secs(5));
        while super::process::live_members(group)
            .unwrap()
            .iter()
            .any(|member| member.pid == self.pid())
        {
            assert!(
                crate::clock::now() < deadline,
                "the process {} did not end",
                self.pid()
            );
            crate::clock::pause(crate::clock::POLL_INTERVAL);
        }
    }

    /// Waits for the process to end and reaps it.
    pub(crate) fn wait_for_exit(&mut self) {
        self.child.wait().unwrap();
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        let host = host_at(self.pid(), 0);
        let _ = System.signal(&host, Signal::KILL);
        let _ = self.child.wait();
    }
}

/// Returns the running process `pid` in the group `group`, as phase records
/// it.
pub(crate) fn recorded(pid: u32, group: u32) -> HostProcess {
    let Start::At(started_at_us) = System.start_time(pid).unwrap() else {
        panic!("the process {pid} has no start time");
    };
    HostProcess {
        pid,
        started_at_us,
        group,
        boot_id: System.boot_id().unwrap(),
    }
}

/// A process of the fake process table.
#[derive(Clone, Debug)]
pub(crate) struct FakeProcess {
    /// What the table holds at its pid.
    pub(crate) start: Start,
    /// The signal that ends it, or `None` for a process that no signal
    /// ends. `KILL` ends a process that `TERM` ends.
    pub(crate) ends_on: Option<Signal>,
    /// For a group leader, whether it leaves members in its group when it
    /// ends, and the signal to the group that ends them, as in
    /// [`FakeProcesses::leaderless`].
    pub(crate) leaves: Option<Option<Signal>>,
}

/// A fake process table.
#[derive(Debug, Default)]
pub(crate) struct FakeProcesses {
    /// The processes by pid.
    pub(crate) processes: RefCell<BTreeMap<u32, FakeProcess>>,
    /// The id of the current boot.
    pub(crate) boot_id: Option<String>,
    /// Each pid that was signalled, with the signal, in order.
    pub(crate) signals: RefCell<Vec<(u32, Signal)>>,
    /// The groups whose leader has ended and that still have members, by
    /// id, with the signal that ends the members, or `None` when no signal
    /// does.
    pub(crate) leaderless: RefCell<BTreeMap<u32, Option<Signal>>>,
}

impl FakeProcesses {
    /// Adds the process `pid`, whose table entry is `start`, which `ends_on`
    /// ends.
    pub(crate) fn with(self, pid: u32, start: Start, ends_on: Option<Signal>) -> Self {
        self.processes.borrow_mut().insert(
            pid,
            FakeProcess {
                start,
                ends_on,
                leaves: None,
            },
        );
        self
    }

    /// Adds the group leader `pid`, as [`FakeProcesses::with`] does, which
    /// leaves members in its group when it ends, and `KILL` to the group
    /// ends them.
    pub(crate) fn with_leaving(self, pid: u32, start: Start, ends_on: Signal) -> Self {
        self.processes.borrow_mut().insert(
            pid,
            FakeProcess {
                start,
                ends_on: Some(ends_on),
                leaves: Some(Some(Signal::KILL)),
            },
        );
        self
    }

    /// Adds the group `group`, whose leader has ended and whose members
    /// `ends_on` ends.
    pub(crate) fn with_leaderless(self, group: u32, ends_on: Option<Signal>) -> Self {
        self.leaderless.borrow_mut().insert(group, ends_on);
        self
    }
}

impl Processes for FakeProcesses {
    fn start_time(&self, pid: u32) -> Result<Start, PhaseError> {
        Ok(self
            .processes
            .borrow()
            .get(&pid)
            .map_or(Start::Absent, |found| found.start))
    }

    fn boot_id(&self) -> Result<Option<String>, PhaseError> {
        Ok(self.boot_id.clone())
    }

    fn group_exists(&self, group: u32) -> bool {
        self.processes.borrow().contains_key(&group)
            || self.leaderless.borrow().contains_key(&group)
    }

    fn reap(&self, _pid: u32) {}

    fn signal(&self, host: &HostProcess, signal: Signal) -> Result<(), PhaseError> {
        self.signals.borrow_mut().push((host.pid, signal));
        let mut processes = self.processes.borrow_mut();
        let ends = processes.get(&host.pid).is_some_and(|found| {
            found.ends_on == Some(signal) || (signal == Signal::KILL && found.ends_on.is_some())
        });
        if ends && let Some(ended) = processes.remove(&host.pid) {
            if let Some(members) = ended.leaves {
                self.leaderless.borrow_mut().insert(host.pid, members);
            }
            return Ok(());
        }
        let mut leaderless = self.leaderless.borrow_mut();
        let members_end = leaderless
            .get(&host.group)
            .is_some_and(|ends_on| *ends_on == Some(signal));
        if host.group == host.pid && members_end {
            leaderless.remove(&host.group);
        }
        Ok(())
    }
}

/// The fake process table stops and identifies processes as the system
/// runner does, and starts, follows and finds nothing.
impl HostRunner for FakeProcesses {
    fn launch(&self, launch: &Launch<'_>) -> Result<Kept, PhaseError> {
        Err(PhaseError::KeeperUnstartable {
            app: launch.args.app().clone(),
            process: launch.args.process().clone(),
            reason: "the fake process table starts nothing".to_owned(),
        })
    }

    fn stop(
        &self,
        stop: &HostStop<'_>,
        deadline: std::time::Instant,
    ) -> Result<HostStopped, PhaseError> {
        process::stop_host(self, stop, deadline)
    }

    fn identity(&self, host: &HostProcess) -> Result<Identity, PhaseError> {
        process::identify(self, host)
    }

    fn follow_log(&self, _log: &std::path::Path) -> Result<Box<dyn LogLines + Send>, PhaseError> {
        Err(PhaseError::ProcessTable {
            message: "the fake process table follows no log".to_owned(),
        })
    }

    fn port_in_use(&self, _port: u16) -> Result<bool, PhaseError> {
        Ok(false)
    }

    fn listener(&self, _group: u32, _port: u16) -> Result<Option<Holder>, PhaseError> {
        Ok(None)
    }
}

/// A Docker object of the fake Docker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FakeObject {
    /// The kind.
    pub(crate) kind: DockerKind,
    /// The name.
    pub(crate) name: String,
    /// The id.
    pub(crate) id: String,
    /// The value of phase's workspace label, or `None` for an object
    /// without it.
    pub(crate) workspace: Option<String>,
    /// The value of phase's run-record label, or `None` for an object
    /// that matches any run record.
    pub(crate) run_record: Option<String>,
    /// The state of a container, which is running unless a test says
    /// otherwise.
    pub(crate) state: ContainerState,
    /// When Docker last started a container, as Docker prints it, which
    /// is a fixed time unless a test says otherwise. `None` stands for a
    /// time that phase cannot read.
    pub(crate) started_at: Option<String>,
}

/// A fake Docker that holds objects in memory.
#[derive(Debug, Default)]
pub(crate) struct FakeDocker {
    /// The objects that exist.
    pub(crate) objects: RefCell<Vec<FakeObject>>,
    /// When true, every call fails, as when the daemon does not answer.
    pub(crate) down: bool,
    /// When true, `remove` succeeds and leaves the object, as a daemon
    /// that fails to remove it does.
    pub(crate) stubborn: bool,
    /// When true, `remove` fails as a daemon that refuses it does.
    pub(crate) refuses: bool,
    /// The ids that `remove` was called with.
    pub(crate) removed: RefCell<Vec<String>>,
    /// The workspace ids and run-record paths whose volume-owner helpers
    /// `remove_volume_helpers` was asked to remove.
    pub(crate) helpers_removed: RefCell<Vec<(String, String)>>,
}

impl FakeDocker {
    /// Adds the object of `kind` named `name` with the id `id`, labelled
    /// with the tests' workspace.
    pub(crate) fn with(self, kind: DockerKind, name: &str, id: &str) -> Self {
        self.labelled(kind, name, id, Some(workspace().as_str()))
    }

    /// Adds the object of `kind` named `name` with the id `id`, whose
    /// workspace label is `label`.
    pub(crate) fn labelled(
        self,
        kind: DockerKind,
        name: &str,
        id: &str,
        label: Option<&str>,
    ) -> Self {
        self.objects.borrow_mut().push(FakeObject {
            kind,
            name: name.to_owned(),
            id: id.to_owned(),
            workspace: label.map(str::to_owned),
            run_record: None,
            state: ContainerState::Running,
            started_at: Some("2026-11-05T10:00:00Z".to_owned()),
        });
        self
    }

    /// Gives the object whose id is `id` the run-record label `run_record`.
    pub(crate) fn run_record(self, id: &str, run_record: &str) -> Self {
        for object in self.objects.borrow_mut().iter_mut() {
            if object.id == id {
                object.run_record = Some(run_record.to_owned());
            }
        }
        self
    }

    /// Sets when Docker last started the container whose id is `id`.
    pub(crate) fn started_at(self, id: &str, started_at: Option<&str>) -> Self {
        for object in self.objects.borrow_mut().iter_mut() {
            if object.id == id {
                object.started_at = started_at.map(str::to_owned);
            }
        }
        self
    }

    /// Sets the state of the container whose id is `id`.
    pub(crate) fn in_state(self, id: &str, state: ContainerState) -> Self {
        for object in self.objects.borrow_mut().iter_mut() {
            if object.id == id {
                object.state = state;
            }
        }
        self
    }

    /// Returns the failure of a daemon that does not answer.
    fn failure() -> DockerError {
        DockerError::unavailable("Cannot connect to the Docker daemon".to_owned())
    }
}

impl FakeDocker {
    /// Returns the object that `object` names, as [`Docker::find`] matches
    /// it.
    fn matching(&self, object: &DockerObject<'_>) -> Result<Option<FakeObject>, DockerError> {
        if self.down {
            return Err(Self::failure());
        }
        let label = object.identity.id.as_str();
        let run_record = object.identity.run_record.to_string_lossy();
        Ok(self
            .objects
            .borrow()
            .iter()
            .find(|found| {
                found.kind == object.kind
                    && found
                        .run_record
                        .as_deref()
                        .is_none_or(|labelled| labelled == run_record)
                    && object.id.map_or(
                        found.name == object.name.as_str()
                            && found.workspace.as_deref() == Some(label),
                        |id| found.id == id,
                    )
            })
            .cloned())
    }
}

impl Docker for FakeDocker {
    fn find(&self, object: &DockerObject<'_>) -> Result<Option<String>, DockerError> {
        Ok(self.matching(object)?.map(|found| found.id))
    }

    fn container_seen(
        &self,
        object: &DockerObject<'_>,
    ) -> Result<Option<SeenContainer>, DockerError> {
        Ok(self
            .matching(object)?
            .map(|found| SeenContainer::new(found.state, Some(0), found.started_at.clone())))
    }

    fn remove(&self, kind: DockerKind, id: &str, _wait: Duration) -> Result<(), DockerError> {
        if self.down {
            return Err(Self::failure());
        }
        if self.refuses {
            return Err(DockerError::refused(format!("container {id} is in use")));
        }
        self.removed.borrow_mut().push(id.to_owned());
        if !self.stubborn {
            self.objects
                .borrow_mut()
                .retain(|found| !(found.kind == kind && found.id == id));
        }
        Ok(())
    }

    fn remove_volume_helpers(&self, identity: &WorkspaceIdentity) -> Result<(), DockerError> {
        if self.down {
            return Err(Self::failure());
        }
        self.helpers_removed.borrow_mut().push((
            identity.id.as_str().to_owned(),
            identity.run_record.to_string_lossy().into_owned(),
        ));
        Ok(())
    }
}

/// A Docker whose every call fails, so a record that names a Docker object
/// makes `sweep` fail, and `sweep` never reports such an object as gone.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Unreachable;

impl Unreachable {
    /// Returns the failure of every call.
    fn failure() -> DockerError {
        DockerError::unavailable("the test's Docker does not answer".to_owned())
    }
}

impl Docker for Unreachable {
    fn find(&self, _object: &DockerObject<'_>) -> Result<Option<String>, DockerError> {
        Err(Self::failure())
    }

    fn container_seen(
        &self,
        _object: &DockerObject<'_>,
    ) -> Result<Option<SeenContainer>, DockerError> {
        Err(Self::failure())
    }

    fn remove(&self, _kind: DockerKind, _id: &str, _wait: Duration) -> Result<(), DockerError> {
        Err(Self::failure())
    }

    fn remove_volume_helpers(&self, _identity: &WorkspaceIdentity) -> Result<(), DockerError> {
        Err(Self::failure())
    }
}

/// Writes `text` to the file at `path`, as a damaged or foreign record
/// would be.
pub(crate) fn write_file(path: &std::path::Path, text: &str) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes a record that phase did not write"
    )]
    std::fs::write(path, text).unwrap();
}
