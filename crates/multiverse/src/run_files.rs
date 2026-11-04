//! The files that mxv keeps in a workspace's run directory: their paths,
//! and reading and writing the JSON records among them.
//!
//! A workspace's run directory is `<runs>/<workspace>/`. Each process or
//! container `<name>` of the workspace has files `<name>.<extension>`
//! there, such as its log `<name>.log`, and `.lock` is the lock that `up`
//! and `down` hold. [`RunDir`] is the one place that builds these paths.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::name::WorkspaceName;
use crate::process;
use crate::store;

#[cfg(test)]
mod tests;

/// The run directory of one workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunDir(PathBuf);

impl RunDir {
    /// Returns the run directory of the workspace `workspace` in the runs
    /// directory `runs`.
    pub(crate) fn new(runs: &Path, workspace: &WorkspaceName) -> Self {
        Self(runs.join(workspace.as_str()))
    }

    /// Returns the directory's path.
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    /// Returns the path of the file of the process or container `unit`
    /// with `extension`, `<unit>.<extension>`.
    pub(crate) fn file(&self, unit: &str, extension: &str) -> PathBuf {
        self.0.join(format!("{unit}.{extension}"))
    }

    /// Returns the log of the process or container `unit`, `<unit>.log`.
    pub(crate) fn log(&self, unit: &str) -> PathBuf {
        self.file(unit, "log")
    }

    /// Returns the older log of the process or container `unit`,
    /// `<unit>.log.1`, which holds the log of its previous start or the
    /// part of its log that a cap moved aside.
    pub(crate) fn older_log(&self, unit: &str) -> PathBuf {
        self.file(unit, "log.1")
    }

    /// Returns the lock file of the workspace, `.lock`.
    pub(crate) fn lock(&self) -> PathBuf {
        self.0.join(".lock")
    }
}

/// The version of the run record that this version of multiverse writes
/// and reads. A run record with a higher version is refused.
pub const RUN_VERSION: u32 = 1;

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

/// A run file in JSON whose version mxv checks.
pub(crate) trait Versioned {
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
pub(crate) fn read_json<T: for<'de> Deserialize<'de> + Versioned>(
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
