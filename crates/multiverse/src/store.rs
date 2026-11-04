//! The state store, which is the one module that writes durable files
//! (DUR-1). It keeps one JSON record for each workspace, named after the
//! workspace, in one directory, and writes each workspace's `AGENTS.md`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};

use serde::Deserialize;

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

    /// Returns every record in the store, in name order. A store whose
    /// directory does not exist holds none.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StateUnreadable`] when the directory or a record
    /// cannot be read, and the errors of [`Store::read`] for a record that
    /// cannot be understood.
    pub fn list(&self) -> Result<Vec<Workspace>, Error> {
        let unreadable = |source| Error::StateUnreadable {
            path: self.dir.clone(),
            source,
        };
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
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
        let mut workspaces = Vec::new();
        for path in paths {
            if let Some(workspace) = read_record(&path)? {
                workspaces.push(workspace);
            }
        }
        Ok(workspaces)
    }

    /// Returns the record of `name`, or `None` when the store has none.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StateUnreadable`] when the record cannot be read,
    /// [`Error::NewerRecord`] when a newer version of multiverse wrote it,
    /// and [`Error::CorruptRecord`] when it is not a record.
    pub fn read(&self, name: &WorkspaceName) -> Result<Option<Workspace>, Error> {
        read_record(&self.path(name))
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

    /// Writes the record of `workspace` over the record with its name.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unwritable`] when the record cannot be written.
    pub fn update(&self, workspace: &Workspace) -> Result<(), Error> {
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
