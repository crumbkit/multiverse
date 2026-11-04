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

/// Reports whether the process group that `record` names is the one that
/// `up` started and still has a member. It is when the process with the
/// recorded id has the recorded start time, and when no process has that
/// id but a group with it still exists: a group keeps its id while any
/// member lives, and no new process can take that id meanwhile, so the
/// group is the one `up` started even after its leader exited. A process
/// with the recorded id and another start time took a free id after the
/// group ended, so the group is not ours.
pub(crate) fn owns_group(record: &RunRecord) -> Result<bool, Error> {
    Ok(match process::started_at(record.pid, &record.started)? {
        Some(owned) => owned,
        None => process::group_exists(record.pid),
    })
}

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
        let no_id = |message: &str| Error::DockerFailed {
            operation: "workspace id",
            container: None,
            message: message.to_owned(),
        };
        let store = self
            .records
            .ok_or_else(|| no_id("mxv has no store of workspace records here"))?;
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

    fn follower_alive(&self, record: &Path) -> Result<bool, Error> {
        let record: Option<RunRecord> = read_json(record)?;
        match record {
            Some(record) => owns_group(&record),
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
