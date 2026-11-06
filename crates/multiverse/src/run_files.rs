//! The files that mxv keeps in a workspace's run directory: reading and
//! writing the JSON records among them, and the [`RunHost`] through which
//! phase's container engine keeps its files, locks and log followers in
//! the same directory.
//!
//! A workspace's run directory is `<runs>/<workspace>/`. Each process or
//! container `<name>` of the workspace has files `<name>.<extension>`
//! there, such as its log `<name>.log`, and `.lock` is the lock that `up`
//! and `down` hold. phase's [`RunDir`] is the one place that builds these
//! paths.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read as _};
use std::path::Path;
use std::time::Duration;

use phase::WorkspaceId;
use phase::container::{Follow, Host};
pub(crate) use phase::container::{RUN_VERSION, RunDir, Versioned};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::name::WorkspaceName;
use crate::ports;
use crate::process::{self, Launch};
use crate::store::{self, Lock, Store};

/// The most bytes of a run record that multiverse reads.
const MOST_RUN_BYTES: u64 = 64 * 1024;

/// The run record of one started process.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RunRecord {
    /// The record's version, which is [`RUN_VERSION`].
    pub(crate) version: u32,
    /// The process id, which is also the id of its process group.
    pub(crate) pid: u32,
    /// The process's start time as `ps` printed it, which tells the
    /// process from a later one that reuses its id.
    pub(crate) started: String,
    /// The ports that the process received, by variable name.
    pub(crate) ports: BTreeMap<String, u16>,
}

/// Whether the process group that a run record names is the one that `up`
/// started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ownership {
    /// The group is the one `up` started and still has a member.
    Ours,
    /// The group's leader has ended and the group still has members, but
    /// nothing proves that they are the ones `up` started. The group may
    /// have emptied after the leader ended and its id passed to another
    /// program's group, so mxv never signals it.
    Unverified,
    /// Nothing of the group that `up` started is left.
    Gone,
}

impl Ownership {
    /// Reports whether the group may still be the one `up` started, so
    /// that mxv keeps its record and its ports and starts nothing in its
    /// place.
    pub(crate) fn may_live(self) -> bool {
        self != Self::Gone
    }
}

/// Returns whether the process group that `record` names is the one that
/// `up` started. It is when the process with the recorded id has the
/// recorded start time. A process with the recorded id and another start
/// time took a free id after the group ended, so the group is gone.
///
/// When no process has the id but a group with it has a member, the group
/// is the one `up` started only when a member started before the shell
/// wrote `exit_file`, less [`CLOCK_SLACK`]: such a member was in the group
/// while the shell ran, and a group keeps its id while it has a member. A
/// group that took the id later has a leader that started after the
/// shell ended. Without that proof the group is unverified.
pub(crate) fn ownership(record: &RunRecord, exit_file: &Path) -> Result<Ownership, Error> {
    Ok(match process::started_at(record.pid, &record.started)? {
        Some(true) => Ownership::Ours,
        Some(false) => Ownership::Gone,
        None if !process::group_exists(record.pid) => Ownership::Gone,
        None if process::member_started_before(record.pid, exit_file, CLOCK_SLACK) => {
            Ownership::Ours
        }
        None => Ownership::Unverified,
    })
}

/// How much earlier than the exit file's time a member must have started
/// for [`ownership`] to count it as the shell's. Linux gives a start time
/// from the boot, which mxv turns into a wall-clock time to within a few
/// hundredths of a second, and the file's time has its own rounding.
const CLOCK_SLACK: Duration = Duration::from_millis(50);

impl Versioned for RunRecord {
    fn version(&self) -> Option<u32> {
        Some(self.version)
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
pub(crate) fn read_json<T: DeserializeOwned + Versioned>(path: &Path) -> Result<Option<T>, Error> {
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
pub(crate) fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), Error> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|source| Error::CorruptRecord {
        path: path.to_path_buf(),
        source,
    })?;
    bytes.push(b'\n');
    store::replace_file(path, &bytes)
}

/// Deletes the file at `path`, and succeeds when it does not exist.
pub(crate) fn remove_if_present(path: &Path) -> Result<(), Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Unwritable {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// The host side of phase's container engine: mxv's run files, store
/// locks, port check and log followers, so the files that the engine
/// writes are the ones that mxv's other commands read, and the store of
/// workspace records that saves each workspace's stable id.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RunHost<'s> {
    /// The store of the workspace records, or `None` for a host that
    /// labels no new docker object.
    pub(crate) records: Option<&'s Store>,
}

impl RunHost<'_> {
    /// The host that labels no new docker object, so it needs no store.
    pub(crate) const WITHOUT_RECORDS: RunHost<'static> = RunHost { records: None };
}

impl Host for RunHost<'_> {
    type Error = Error;
    type Lock = Lock;

    fn read_json<T: DeserializeOwned + Versioned>(&self, path: &Path) -> Result<Option<T>, Error> {
        read_json(path)
    }

    fn write_json<T: Serialize>(&self, path: &Path, value: &T) -> Result<(), Error> {
        write_json(path, value)
    }

    fn remove_file(&self, path: &Path) -> Result<(), Error> {
        remove_if_present(path)
    }

    fn rotate_log(&self, log: &Path, older: &Path) -> Result<(), Error> {
        store::rotate_log(log, older)
    }

    fn lock(&self, path: &Path, wait: Duration) -> Result<Lock, Error> {
        store::lock(path, wait)
    }

    fn saved_workspace_id(&self, workspace: &WorkspaceName) -> Result<WorkspaceId, Error> {
        let no_id = |reason: &str| Error::DockerFailed {
            operation: "workspace id".to_owned(),
            container: None,
            message: format!("mxv cannot name the workspace's containers, because {reason}"),
            docker_line: None,
        };
        let store = self
            .records
            .ok_or_else(|| no_id("it has no store of workspace records here"))?;
        let mut record = store
            .read(workspace)?
            .ok_or_else(|| no_id("the workspace has no record"))?;
        if record.id.is_none() {
            store.update(&mut record)?;
        }
        record
            .id
            .ok_or_else(|| no_id("the workspace's record holds no stable id"))
    }

    fn port_taken(&self, port: u16) -> bool {
        ports::is_taken(port)
    }

    fn follower_alive(&self, record_path: &Path) -> Result<bool, Error> {
        let record: Option<RunRecord> = read_json(record_path)?;
        match record {
            // The follower's shell writes its exit code beside its record,
            // as `<service>.logs-exit` beside `<service>.follower`.
            Some(record) => {
                Ok(ownership(&record, &record_path.with_extension("logs-exit"))?.may_live())
            }
            None => Ok(false),
        }
    }

    fn follow(&self, follow: &Follow<'_>) -> Result<(), Error> {
        let follower = process::launch(&Launch {
            name: follow.name,
            argv: follow.argv,
            cwd: follow.cwd,
            env: &[],
            log: follow.log,
            exit_file: follow.exit_file,
        })?;
        // A follower whose record cannot be written stops with its
        // handle, so mxv never leaves a follower that it does not know of.
        let pid = follower.pid();
        let started = process::start_time(pid)?.unwrap_or_default();
        write_json(
            follow.record,
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
}
