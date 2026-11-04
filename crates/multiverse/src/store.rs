//! The state store, which is the one module that writes durable files
//! (DUR-1). It keeps one JSON record for each workspace, named after the
//! workspace, in one directory, and writes each workspace's `AGENTS.md`.
//!
//! It also owns the advisory locks that keep two mxv commands from starting
//! or stopping one workspace's app, or allocating ports, at the same time.
//! A lock is an exclusive `flock` on a file. The operating system releases
//! it when the file closes, which happens when the [`Lock`] drops or the
//! process that holds it ends, so a crashed mxv leaves no stale lock.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::clock::{POLL_INTERVAL, pause};
use crate::error::Error;
use crate::name::WorkspaceName;
use crate::record::{RECORD_VERSION, Workspace};

#[cfg(test)]
mod tests;

/// The most bytes of a record that the store reads. A larger record is
/// reported as corrupt.
pub const MOST_RECORD_BYTES: u64 = 1024 * 1024;

/// The extension of a record file.
const EXTENSION: &str = "json";

/// The directory of workspace records.
#[derive(Clone, Debug)]
pub struct Store {
    /// The directory that holds one `<name>.json` for each workspace.
    dir: PathBuf,
}

/// The outcome of [`Store::create`].
#[derive(Debug, PartialEq, Eq)]
pub enum Created {
    /// The store wrote the record.
    Written,
    /// A record with the workspace's name already exists, so the store left
    /// it unchanged.
    Taken,
}

/// The records that [`Store::list`] found.
#[derive(Debug)]
#[non_exhaustive]
pub struct Records {
    /// Each record that the store could read, in name order.
    pub workspaces: Vec<Workspace>,
    /// The error of each record that the store could not read, such as
    /// [`Error::CorruptRecord`] or [`Error::NewerRecord`], in name order.
    pub problems: Vec<Error>,
}

/// The part of a record that the store reads first, so a record of a newer
/// version is refused before its other fields are read.
#[derive(Deserialize)]
struct Versioned {
    /// The record's version.
    version: u32,
}

impl Store {
    /// Returns the store whose records live in `dir`, such as
    /// `~/.local/state/mxv/workspaces`. The store creates `dir` on its
    /// first write.
    #[must_use]
    pub const fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Returns the directory of the records.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Returns the path of the record of `name`.
    #[must_use]
    pub fn path(&self, name: &WorkspaceName) -> PathBuf {
        self.dir.join(format!("{name}.{EXTENSION}"))
    }

    /// Returns every record in the store, in name order, and the error of
    /// each record that cannot be read, so one bad record does not hide
    /// the others. A store whose directory does not exist holds none.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StateUnreadable`] when the directory cannot be
    /// read.
    pub fn list(&self) -> Result<Records, Error> {
        let unreadable = |source| Error::StateUnreadable {
            path: self.dir.clone(),
            source,
        };
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Records {
                    workspaces: Vec::new(),
                    problems: Vec::new(),
                });
            }
            Err(error) => return Err(unreadable(error)),
        };
        let mut paths = Vec::new();
        for entry in entries {
            let path = entry.map_err(unreadable)?.path();
            let is_record = path
                .extension()
                .is_some_and(|extension| extension == EXTENSION)
                && path
                    .file_name()
                    .is_some_and(|name| !name.to_string_lossy().starts_with('.'));
            if is_record {
                paths.push(path);
            }
        }
        paths.sort();
        let mut records = Records {
            workspaces: Vec::new(),
            problems: Vec::new(),
        };
        for path in paths {
            let stem = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            match read_record(&path).and_then(|found| matching(&path, &stem, found)) {
                Ok(Some(workspace)) => records.workspaces.push(workspace),
                Ok(None) => {}
                Err(error) => records.problems.push(error),
            }
        }
        Ok(records)
    }

    /// Returns the record of `name`, or `None` when the store has none.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StateUnreadable`] when the record cannot be read,
    /// [`Error::NewerRecord`] when a newer version of multiverse wrote it,
    /// [`Error::CorruptRecord`] when it is not a record, and
    /// [`Error::MismatchedRecord`] when it holds another name.
    pub fn read(&self, name: &WorkspaceName) -> Result<Option<Workspace>, Error> {
        let path = self.path(name);
        let found = read_record(&path)?;
        matching(&path, name.as_str(), found)
    }

    /// Writes the record of `workspace` unless the store already holds a
    /// record with its name. Two runs that create the same name at once
    /// write one record, and the other run reads [`Created::Taken`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unwritable`] when the directory or the record
    /// cannot be written.
    pub fn create(&self, workspace: &Workspace) -> Result<Created, Error> {
        let target = self.path(&workspace.name);
        let bytes = record_bytes(workspace, &target)?;
        let unwritable = |source| Error::Unwritable {
            path: target.clone(),
            source,
        };
        fs::create_dir_all(&self.dir).map_err(unwritable)?;
        let temp = write_temp(&target, &bytes)?;
        // A hard link fails when the target exists, so it creates the
        // record in one step without replacing another run's record.
        let linked = fs::hard_link(&temp, &target);
        let _ = fs::remove_file(&temp);
        match linked {
            Ok(()) => {
                sync_dir(&self.dir);
                Ok(Created::Written)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(Created::Taken),
            Err(error) => Err(unwritable(error)),
        }
    }

    /// Writes the record of `workspace` over the record with its name. A
    /// workspace whose record an earlier version wrote, which has no stable
    /// id, gets one first, so the record on disk holds the id before any
    /// docker object is labelled with it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unwritable`] when the record cannot be written, and
    /// the errors of [`Workspace::ensure_id`].
    pub fn update(&self, workspace: &mut Workspace) -> Result<(), Error> {
        workspace.ensure_id()?;
        let target = self.path(&workspace.name);
        let bytes = record_bytes(workspace, &target)?;
        replace_file(&target, &bytes)
    }

    /// Deletes the record of `name`. Deleting a record that does not exist
    /// succeeds.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unwritable`] when the record cannot be deleted.
    pub fn delete(&self, name: &WorkspaceName) -> Result<(), Error> {
        let path = self.path(name);
        match fs::remove_file(&path) {
            Ok(()) => {
                sync_dir(&self.dir);
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(Error::Unwritable { path, source }),
        }
    }
}

/// Returns the JSON of `workspace`, ending with a line feed.
fn record_bytes(workspace: &Workspace, target: &Path) -> Result<Vec<u8>, Error> {
    let mut bytes =
        serde_json::to_vec_pretty(workspace).map_err(|source| Error::CorruptRecord {
            path: target.to_path_buf(),
            source,
        })?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Returns `found`, the record at `path`, when it holds the name `name`
/// that its file is named for, and [`Error::MismatchedRecord`] otherwise,
/// such as when a file system that ignores case opened `App.json` for
/// `app.json`.
fn matching(path: &Path, name: &str, found: Option<Workspace>) -> Result<Option<Workspace>, Error> {
    match found {
        Some(workspace) if workspace.name.as_str() != name => Err(Error::MismatchedRecord {
            path: path.to_path_buf(),
            name: workspace.name.to_string(),
        }),
        found => Ok(found),
    }
}

/// Returns the record at `path`, or `None` when no file is there.
fn read_record(path: &Path) -> Result<Option<Workspace>, Error> {
    let unreadable = |source| Error::StateUnreadable {
        path: path.to_path_buf(),
        source,
    };
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(unreadable(error)),
    };
    let mut bytes = Vec::new();
    file.take(MOST_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    let corrupt = |source| Error::CorruptRecord {
        path: path.to_path_buf(),
        source,
    };
    let versioned: Versioned = serde_json::from_slice(&bytes).map_err(corrupt)?;
    if versioned.version > RECORD_VERSION {
        return Err(Error::NewerRecord {
            path: path.to_path_buf(),
            version: versioned.version,
        });
    }
    serde_json::from_slice(&bytes).map(Some).map_err(corrupt)
}

/// Writes `bytes` to the file at `path`, replacing any file there in one
/// step: it writes a temporary file beside `path`, syncs it, renames it
/// over `path` and syncs the directory (DUR-1).
///
/// # Errors
///
/// Returns [`Error::Unwritable`] when a step fails. The temporary file is
/// removed after a failed rename.
pub(crate) fn replace_file(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let temp = write_temp(path, bytes)?;
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/store.rs is the module that writes durable files (DUR-1)"
    )]
    let renamed = fs::rename(&temp, path);
    if let Err(source) = renamed {
        let _ = fs::remove_file(&temp);
        return Err(Error::Unwritable {
            path: path.to_path_buf(),
            source,
        });
    }
    if let Some(dir) = path.parent() {
        sync_dir(dir);
    }
    Ok(())
}

/// Writes `bytes` to a new temporary file beside `target`, syncs it and
/// returns its path. The name starts with `.`, so [`Store::list`] skips it.
fn write_temp(target: &Path, bytes: &[u8]) -> Result<PathBuf, Error> {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = target.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let unwritable = |source| Error::Unwritable {
        path: temp.clone(),
        source,
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/store.rs is the module that writes durable files (DUR-1)"
    )]
    let opened = OpenOptions::new().write(true).create_new(true).open(&temp);
    let mut file = opened.map_err(unwritable)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    if let Err(error) = written {
        let _ = fs::remove_file(&temp);
        return Err(unwritable(error));
    }
    Ok(temp)
}

/// Syncs the directory `dir`, so a rename or a removal in it survives a
/// power loss. A platform that cannot open a directory as a file, such as
/// Windows, skips the sync, because it has no other way to sync one.
fn sync_dir(dir: &Path) {
    if let Ok(handle) = File::open(dir) {
        let _ = handle.sync_all();
    }
}

/// Opens the file at `path` for appending, and creates it and its
/// directory when they do not exist. An app's process writes its log
/// through the handle, so the log grows in place and is never replaced.
///
/// # Errors
///
/// Returns [`Error::Unwritable`] when the directory or the file cannot be
/// created or opened.
pub(crate) fn append_file(path: &Path) -> Result<File, Error> {
    let unwritable = |source| Error::Unwritable {
        path: path.to_path_buf(),
        source,
    };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(unwritable)?;
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/store.rs is the module that writes durable files (DUR-1)"
    )]
    let opened = OpenOptions::new().create(true).append(true).open(path);
    opened.map_err(unwritable)
}

/// Moves the log at `log` to `older`, replacing an older log there, so the
/// next start of the process begins an empty log. A missing log succeeds.
///
/// # Errors
///
/// Returns [`Error::Unwritable`] when the rename fails.
pub(crate) fn rotate_log(log: &Path, older: &Path) -> Result<(), Error> {
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/store.rs is the module that writes durable files (DUR-1)"
    )]
    let renamed = fs::rename(log, older);
    match renamed {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Unwritable {
            path: log.to_path_buf(),
            source,
        }),
    }
}

/// Copies the log at `log` to `older` and empties it when it holds more
/// than `most` bytes. The process keeps writing to `log`, because it opened
/// the file to append, so each later write lands at the new end. Lines that
/// the process writes between the copy and the emptying are lost.
///
/// # Errors
///
/// Returns [`Error::Unwritable`] when the copy or the emptying fails.
pub(crate) fn cap_log(log: &Path, older: &Path, most: u64) -> Result<(), Error> {
    let unwritable = |source| Error::Unwritable {
        path: log.to_path_buf(),
        source,
    };
    let Ok(metadata) = fs::metadata(log) else {
        return Ok(());
    };
    if metadata.len() <= most {
        return Ok(());
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/store.rs is the module that writes durable files (DUR-1)"
    )]
    let copied = fs::copy(log, older);
    copied.map_err(unwritable)?;
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/store.rs is the module that writes durable files (DUR-1)"
    )]
    let opened = OpenOptions::new().write(true).open(log);
    opened.map_err(unwritable)?.set_len(0).map_err(unwritable)
}

/// How long a command waits for a lock that another command holds before
/// it fails with [`Error::Busy`].
pub const LOCK_WAIT: Duration = Duration::from_secs(5);

/// A held lock, which the operating system releases when it drops.
#[derive(Debug)]
#[must_use = "the lock is released when it drops"]
pub struct Lock {
    /// The locked file, which holds the lock while it is open.
    _file: File,
}

/// Takes the exclusive lock on the file at `path`, creating the file and
/// its directory when they do not exist. When another holder has the
/// lock, it tries again every [`POLL_INTERVAL`] until `wait` passes.
///
/// # Errors
///
/// Returns [`Error::Busy`] when another holder keeps the lock for all of
/// `wait`, and [`Error::Unwritable`] when the file cannot be opened or
/// locked.
pub(crate) fn lock(path: &Path, wait: Duration) -> Result<Lock, Error> {
    let file = append_file(path)?;
    let deadline = Instant::now() + wait;
    loop {
        #[expect(
            clippy::disallowed_methods,
            reason = "crates/multiverse/src/store.rs owns file locks and their deadline"
        )]
        let attempt = file.try_lock();
        match attempt {
            Ok(()) => return Ok(Lock { _file: file }),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(source)) => {
                return Err(Error::Unwritable {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }
        if Instant::now() >= deadline {
            return Err(Error::Busy {
                path: PathBuf::from(path),
            });
        }
        pause(POLL_INTERVAL);
    }
}
