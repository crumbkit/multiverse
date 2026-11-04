//! Helpers for the tests of the run record and `sweep`: real processes that
//! the tests start and record, a fake process table, and a fake Docker.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::time::Duration;

use rustix::process::Signal;

use crate::error::PhaseError;
use crate::name::{AppName, ProcessName, WorkspaceId};
use crate::record::docker::{Docker, DockerError, DockerKind, DockerObject};
use crate::record::process::{Processes, Start, System};
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
        #[expect(
            clippy::disallowed_methods,
            reason = "the test starts a real process for sweep to stop"
        )]
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", script])
            .process_group(0)
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
}

impl FakeProcesses {
    /// Adds the process `pid`, whose table entry is `start`, which `ends_on`
    /// ends.
    pub(crate) fn with(self, pid: u32, start: Start, ends_on: Option<Signal>) -> Self {
        self.processes
            .borrow_mut()
            .insert(pid, FakeProcess { start, ends_on });
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
    }

    fn reap(&self, _pid: u32) {}

    fn signal(&self, host: &HostProcess, signal: Signal) -> Result<(), PhaseError> {
        self.signals.borrow_mut().push((host.pid, signal));
        let mut processes = self.processes.borrow_mut();
        let ends = processes.get(&host.pid).is_some_and(|found| {
            found.ends_on == Some(signal) || (signal == Signal::KILL && found.ends_on.is_some())
        });
        if ends {
            processes.remove(&host.pid);
        }
        Ok(())
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
        });
        self
    }

    /// Returns the failure of a daemon that does not answer.
    fn failure() -> DockerError {
        DockerError::unavailable("Cannot connect to the Docker daemon".to_owned())
    }
}

impl Docker for FakeDocker {
    fn find(&self, object: &DockerObject<'_>) -> Result<Option<String>, DockerError> {
        if self.down {
            return Err(Self::failure());
        }
        let label = object.workspace.as_str();
        Ok(self
            .objects
            .borrow()
            .iter()
            .find(|found| {
                found.kind == object.kind
                    && object.id.map_or(
                        found.name == object.name.as_str()
                            && found.workspace.as_deref() == Some(label),
                        |id| found.id == id,
                    )
            })
            .map(|found| found.id.clone()))
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

    fn remove(&self, _kind: DockerKind, _id: &str, _wait: Duration) -> Result<(), DockerError> {
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
