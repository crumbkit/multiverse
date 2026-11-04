//! Helpers for the tests of the run record and `sweep`: real processes that
//! the tests start and record, and a fake Docker.

use std::cell::RefCell;
use std::io::{BufRead as _, BufReader};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::name::{AppName, ProcessName, WorkspaceId};
use crate::record::docker::{Docker, DockerError, DockerKind, DockerObject};
use crate::record::process::{self, Start};
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
}

impl Spawned {
    /// Starts `sh -c script` in a new process group, and waits until it
    /// prints a line, which the script prints once it is ready.
    pub(crate) fn shell(script: &str) -> Self {
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
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        Self { child }
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
        let Start::At(started_at_us) = process::start_time(self.pid()).unwrap() else {
            panic!("the process that the test started has no start time");
        };
        HostProcess {
            pid: self.pid(),
            started_at_us,
            group: self.pid(),
        }
    }

    /// Reports whether the process is still running. A process that
    /// `sweep` has reaped is no longer this test's child, and is not.
    pub(crate) fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = process::signal(self.pid(), self.pid(), rustix::process::Signal::KILL);
        let _ = self.child.wait();
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
    /// The ids that `remove` was called with.
    pub(crate) removed: RefCell<Vec<String>>,
}

impl FakeDocker {
    /// Adds the object of `kind` named `name` with the id `id`.
    pub(crate) fn with(self, kind: DockerKind, name: &str, id: &str) -> Self {
        self.objects.borrow_mut().push(FakeObject {
            kind,
            name: name.to_owned(),
            id: id.to_owned(),
        });
        self
    }

    /// Returns the failure of a daemon that does not answer.
    fn failure() -> DockerError {
        DockerError::new("Cannot connect to the Docker daemon".to_owned())
    }
}

impl Docker for FakeDocker {
    fn find(&self, object: &DockerObject<'_>) -> Result<Option<String>, DockerError> {
        if self.down {
            return Err(Self::failure());
        }
        Ok(self
            .objects
            .borrow()
            .iter()
            .find(|found| {
                found.kind == object.kind
                    && object
                        .id
                        .map_or(found.name == object.name.as_str(), |id| found.id == id)
            })
            .map(|found| found.id.clone()))
    }

    fn remove(&self, kind: DockerKind, id: &str, _wait: Duration) -> Result<(), DockerError> {
        if self.down {
            return Err(Self::failure());
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

/// Writes `text` to the file at `path`, as a damaged or foreign record
/// would be.
pub(crate) fn write_file(path: &std::path::Path, text: &str) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes a record that phase did not write"
    )]
    std::fs::write(path, text).unwrap();
}
