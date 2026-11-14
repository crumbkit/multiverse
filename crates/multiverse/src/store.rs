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
use sha2::{Digest as _, Sha256};

use crate::archive::{ArchiveId, MintHeld, mint_held};
use crate::clock::{POLL_INTERVAL, pause};
use crate::error::Error;
use crate::name::WorkspaceName;
use crate::record::{RECORD_VERSION, Workspace, WorkspaceState};

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
    /// write one record, and the other run reads [`Created::Taken`]. A
    /// workspace without a stable id gets one first, so every record that
    /// the store creates holds one. A workspace with a checkout that does
    /// not match its kind is refused before anything changes, in memory or
    /// on disk.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MismatchedCheckout`] when a checkout does not match
    /// the workspace's kind, [`Error::Unwritable`] when the directory or the
    /// record cannot be written, and the errors of [`Workspace::ensure_id`].
    pub fn create(&self, workspace: &mut Workspace) -> Result<Created, Error> {
        workspace.check_checkouts()?;
        workspace.ensure_id()?;
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

    /// Writes the record of `workspace` over the record with its name, and
    /// writes nothing when no record of the name exists, because only
    /// [`Store::create`] makes a record. A command that read the record
    /// before another command archived or removed the workspace therefore
    /// cannot write it back. It never changes the workspace's state, which
    /// only [`Store::advance`] does, and writes nothing when the record on
    /// disk holds another state than `workspace`, such as when `mxv new`
    /// finished the workspace after this command read it. A workspace whose
    /// record an earlier version wrote, which has no stable id, gets one
    /// first, so the record on disk holds the id before any docker object
    /// is labelled with it. The exception is a record without an id beside
    /// an archive record of its name that an older mxv left retiring
    /// without a workspace id, or that cannot be read: the update mints no
    /// id, so the archive and the record still read as one workspace, and
    /// it raises the version only as far as the record's checkouts need. A
    /// workspace with a checkout that does not match its kind is refused
    /// before anything changes, in memory or on disk.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MismatchedCheckout`] when a checkout does not match
    /// the workspace's kind, [`Error::RecordMissing`] when no record of the
    /// name exists, [`Error::StateChanged`] when the record on disk holds
    /// another state, the errors of [`Store::read`] when the record on disk
    /// cannot be read, [`Error::Unwritable`] when the record cannot be
    /// written, and the errors of [`Workspace::ensure_id`].
    pub fn update(&self, workspace: &mut Workspace) -> Result<(), Error> {
        workspace.check_checkouts()?;
        self.check_stored(workspace)?;
        if workspace.id.is_none() && self.mint_held(&workspace.name).is_some() {
            workspace.raise_version();
            let target = self.path(&workspace.name);
            let bytes = record_bytes(workspace, &target)?;
            return replace_file(&target, &bytes);
        }
        self.write(workspace)
    }

    /// Returns why an update of the record of `name` must not mint a stable
    /// id, or `None` when it may. The archives lie in `archives`, beside
    /// the store's directory.
    pub(crate) fn mint_held(&self, name: &WorkspaceName) -> Option<MintHeld> {
        let archives = self.dir.parent()?.join("archives");
        mint_held(&archives, name)
    }

    /// Moves the workspace to the state `to` and writes its record. It
    /// makes two moves: from [`WorkspaceState::Creating`] to
    /// [`WorkspaceState::Live`], which `mxv new` makes with its last write,
    /// and from [`WorkspaceState::Live`] to [`WorkspaceState::Retiring`],
    /// which `mxv archive` makes before it writes the archive record. A
    /// move from retiring into an archive to retiring into the same
    /// archive writes nothing, so a resumed archive may make it again.
    /// Like [`Store::update`], it writes nothing when no record of the name
    /// exists or the record on disk holds another state than `workspace`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TransitionRefused`] for any other move, and the
    /// errors of [`Store::update`]. `workspace` keeps its state after a
    /// failure.
    pub fn advance(&self, workspace: &mut Workspace, to: WorkspaceState) -> Result<(), Error> {
        let from = workspace.state();
        match (&from, &to) {
            (WorkspaceState::Creating, WorkspaceState::Live)
            | (WorkspaceState::Live, WorkspaceState::Retiring { .. }) => {}
            (WorkspaceState::Retiring { archive: held }, WorkspaceState::Retiring { archive })
                if held == archive =>
            {
                return self.check_stored(workspace);
            }
            _ => {
                return Err(Error::TransitionRefused {
                    name: workspace.name.clone(),
                    from,
                    to,
                });
            }
        }
        self.move_to(workspace, to)
    }

    /// Moves a retiring workspace to retiring into the archive `archive`,
    /// for `mxv archive` when the archive record that an earlier run wrote
    /// carries another id than the workspace's state names.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TransitionRefused`] when the workspace is not
    /// retiring, and the errors of [`Store::update`]. `workspace` keeps its
    /// state after a failure.
    pub fn repoint(&self, workspace: &mut Workspace, archive: &ArchiveId) -> Result<(), Error> {
        let to = WorkspaceState::retiring(archive.clone());
        let from = workspace.state();
        if from.archive().is_none() {
            return Err(Error::TransitionRefused {
                name: workspace.name.clone(),
                from,
                to,
            });
        }
        self.move_to(workspace, to)
    }

    /// Writes the record of `workspace` in the state `to`, once its
    /// checkouts match its kind and the record on disk holds the state that
    /// `workspace` holds.
    fn move_to(&self, workspace: &mut Workspace, to: WorkspaceState) -> Result<(), Error> {
        workspace.check_checkouts()?;
        self.check_stored(workspace)?;
        let from = workspace.state();
        workspace.set_state(to);
        let written = self.write(workspace);
        if written.is_err() {
            workspace.set_state(from);
        }
        written
    }

    /// Returns an error unless the store holds a record of `workspace`'s
    /// name in the state that `workspace` holds.
    fn check_stored(&self, workspace: &Workspace) -> Result<(), Error> {
        // The check and the rename in `replace_file` are two steps. A
        // caller that holds the workspace's lock is safe, because an
        // archive deletes the record under that lock. `mxv new` and
        // `mxv remove` write and delete the record without the lock, so a
        // record that one of them changes between the check and the rename
        // is still overwritten. Making them take the workspace's lock
        // closes this window.
        match self.read(&workspace.name)? {
            None => Err(Error::RecordMissing {
                name: workspace.name.clone(),
            }),
            Some(stored) if stored.state() != workspace.state() => Err(Error::StateChanged {
                name: workspace.name.clone(),
            }),
            Some(_) => Ok(()),
        }
    }

    /// Writes the record of `workspace` over the record with its name.
    fn write(&self, workspace: &mut Workspace) -> Result<(), Error> {
        let target = self.path(&workspace.name);
        workspace.ensure_id()?;
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
pub(crate) fn sync_dir(dir: &Path) {
    if let Ok(handle) = File::open(dir) {
        let _ = handle.sync_all();
    }
}

/// One entry of a directory that [`entries`] lists.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Entry {
    /// The entry's name.
    pub(crate) name: String,
    /// The entry's path.
    pub(crate) path: PathBuf,
    /// Whether the entry is a directory, without following a symbolic link.
    pub(crate) dir: bool,
}

/// Returns each entry of `dir` whose name is text, in name order, or
/// nothing when `dir` does not exist.
///
/// # Errors
///
/// Returns [`Error::StateUnreadable`] when `dir` cannot be read.
pub(crate) fn entries(dir: &Path) -> Result<Vec<Entry>, Error> {
    let unreadable = |source| Error::StateUnreadable {
        path: dir.to_path_buf(),
        source,
    };
    let listed = match fs::read_dir(dir) {
        Ok(listed) => listed,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(unreadable(error)),
    };
    let mut found = Vec::new();
    for entry in listed {
        let entry = entry.map_err(unreadable)?;
        let dir = entry.file_type().map_err(unreadable)?.is_dir();
        if let Ok(name) = entry.file_name().into_string() {
            found.push(Entry {
                name,
                path: entry.path(),
                dir,
            });
        }
    }
    found.sort();
    Ok(found)
}

/// Copies the file at `from` to `to` in one step, with the permission bits
/// `mode`: it streams the bytes to a temporary file beside `to`, syncs it,
/// renames it over `to` and syncs the directory (DUR-1). It returns the
/// file's SHA-256 digest in lowercase hex, computed from the bytes it
/// wrote.
///
/// # Errors
///
/// Returns [`Error::StateUnreadable`] when `from` cannot be read, and
/// [`Error::Unwritable`] when a step of the write fails. The temporary file
/// is removed after a failure.
pub(crate) fn copy_file(from: &Path, to: &Path, mode: u32) -> Result<String, Error> {
    let mut source = File::open(from).map_err(|source| Error::StateUnreadable {
        path: from.to_path_buf(),
        source,
    })?;
    write_from(&mut source, from, to, mode)
}

/// Writes the bytes that `source` reads to `to` in one step, with the
/// permission bits `mode`, as [`copy_file`] does, and returns their SHA-256
/// digest in lowercase hex. `from` names the source in an error.
///
/// # Errors
///
/// Returns [`Error::StateUnreadable`] with `from` when `source` fails, and
/// [`Error::Unwritable`] when a step of the write fails. The temporary file
/// is removed after a failure.
pub(crate) fn write_from(
    source: &mut impl io::Read,
    from: &Path,
    to: &Path,
    mode: u32,
) -> Result<String, Error> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let unreadable = |source| Error::StateUnreadable {
        path: from.to_path_buf(),
        source,
    };
    let name = to
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = to.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let unwritable = |source| Error::Unwritable {
        path: temp.clone(),
        source,
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/store.rs is the module that writes durable files (DUR-1)"
    )]
    let opened = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temp);
    let mut file = opened.map_err(unwritable)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    let copied: Result<(), Error> = loop {
        let count = match source.read(&mut buffer) {
            Ok(0) => break Ok(()),
            Ok(count) => count,
            Err(error) => break Err(unreadable(error)),
        };
        let chunk = buffer.get(..count).unwrap_or_default();
        hasher.update(chunk);
        if let Err(error) = file.write_all(chunk) {
            break Err(unwritable(error));
        }
    };
    let synced = copied.and_then(|()| {
        // The umask may have narrowed `mode` when the file was created.
        file.set_permissions(fs::Permissions::from_mode(mode))
            .and_then(|()| file.sync_all())
            .map_err(unwritable)
    });
    if let Err(error) = synced {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/store.rs is the module that writes durable files (DUR-1)"
    )]
    let renamed = fs::rename(&temp, to);
    if let Err(source) = renamed {
        let _ = fs::remove_file(&temp);
        return Err(Error::Unwritable {
            path: to.to_path_buf(),
            source,
        });
    }
    if let Some(dir) = to.parent() {
        sync_dir(dir);
    }
    Ok(hex_digest(&hasher.finalize()))
}

/// Writes a new file at `to` in one step, with the permission bits `mode`:
/// `fill` writes the bytes into a temporary file `.<name>.<pid>.tmp` beside
/// `to`, which is synced and renamed over `to`, and then the directory is
/// synced (DUR-1). It first removes each temporary file of `to` that an
/// earlier run left when it stopped partway, whatever its process id.
///
/// # Errors
///
/// Returns the failure of `fill`, and [`Error::Unwritable`] when a step of
/// the write fails. The temporary file is removed after a failure.
pub(crate) fn write_file_with(
    to: &Path,
    mode: u32,
    fill: impl FnOnce(&mut File) -> Result<(), Error>,
) -> Result<(), Error> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let name = to
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dir = to.parent().unwrap_or_else(|| Path::new("."));
    remove_stale_temps(dir, &name)?;
    let temp = to.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let unwritable = |source| Error::Unwritable {
        path: temp.clone(),
        source,
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/store.rs is the module that writes durable files (DUR-1)"
    )]
    let opened = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temp);
    let mut file = opened.map_err(unwritable)?;
    let written = fill(&mut file).and_then(|()| {
        // The umask may have narrowed `mode` when the file was created.
        file.set_permissions(fs::Permissions::from_mode(mode))
            .and_then(|()| file.sync_all())
            .map_err(unwritable)
    });
    if let Err(error) = written {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/store.rs is the module that writes durable files (DUR-1)"
    )]
    let renamed = fs::rename(&temp, to);
    if let Err(source) = renamed {
        let _ = fs::remove_file(&temp);
        return Err(Error::Unwritable {
            path: to.to_path_buf(),
            source,
        });
    }
    sync_dir(dir);
    Ok(())
}

/// Removes each temporary file `.<name>.<pid>.tmp` in `dir`.
fn remove_stale_temps(dir: &Path, name: &str) -> Result<(), Error> {
    let prefix = format!(".{name}.");
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(Error::StateUnreadable {
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    for entry in entries.flatten() {
        let found = entry.file_name();
        let Some(found) = found.to_str() else {
            continue;
        };
        let pid = found
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(".tmp"));
        if pid.is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit())) {
            let path = entry.path();
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(source) => return Err(Error::Unwritable { path, source }),
            }
        }
    }
    Ok(())
}

/// Returns the SHA-256 digest of the file at `path` in lowercase hex.
///
/// # Errors
///
/// Returns [`Error::StateUnreadable`] when the file cannot be read.
pub(crate) fn file_digest(path: &Path) -> Result<String, Error> {
    let unreadable = |source| Error::StateUnreadable {
        path: path.to_path_buf(),
        source,
    };
    let mut file = File::open(path).map_err(unreadable)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(unreadable)?;
        if count == 0 {
            return Ok(hex_digest(&hasher.finalize()));
        }
        hasher.update(buffer.get(..count).unwrap_or_default());
    }
}

/// Returns `bytes` in lowercase hex.
pub(crate) fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Makes the directory `path` and its parents when they do not exist, gives
/// `path` the permission bits 0700, and syncs its parent, so the directory
/// survives a power loss and only its owner can read it.
///
/// # Errors
///
/// Returns [`Error::Unwritable`] when the directory cannot be made or its
/// permissions set.
pub(crate) fn private_dir(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt as _;
    let unwritable = |source| Error::Unwritable {
        path: path.to_path_buf(),
        source,
    };
    fs::create_dir_all(path).map_err(unwritable)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(unwritable)?;
    if let Some(parent) = path.parent() {
        sync_dir(parent);
    }
    Ok(())
}

/// Moves the directory or link at `from` to `to` in one rename. The move
/// writes no bytes of its own, so a power loss leaves it at one of the two
/// names.
///
/// # Errors
///
/// Returns the failure of the rename, with the path it could not move.
pub(crate) fn move_aside(from: &Path, to: &Path) -> Result<(), Error> {
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/store.rs is the module that writes durable files (DUR-1)"
    )]
    let moved = fs::rename(from, to);
    moved.map_err(|source| Error::Unwritable {
        path: from.to_path_buf(),
        source,
    })?;
    if let Some(dir) = to.parent() {
        sync_dir(dir);
    }
    Ok(())
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

/// Sets the modification time of the file at `path` to now, and creates
/// it and its directory when they do not exist. The file holds no bytes,
/// and nothing syncs it, because a time that a power loss takes back only
/// makes the time older.
///
/// # Errors
///
/// Returns [`Error::Unwritable`] when the directory or the file cannot be
/// created, or its time cannot be set.
pub(crate) fn touch_file(path: &Path) -> Result<(), Error> {
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
    opened
        .and_then(|file| file.set_modified(std::time::SystemTime::now()))
        .map_err(unwritable)
}

/// Creates the empty file at `path` with the permission bits `mode`, and
/// syncs its directory. It never replaces an entry: it returns `false` and
/// changes nothing when something already has the name, which may be a
/// directory or a link.
///
/// # Errors
///
/// Returns [`Error::Unwritable`] when the file cannot be created or its
/// permission bits cannot be set. A file that was created before the
/// failure is removed.
pub(crate) fn create_empty(path: &Path, mode: u32) -> Result<bool, Error> {
    use std::os::unix::fs::PermissionsExt as _;
    let unwritable = |source| Error::Unwritable {
        path: path.to_path_buf(),
        source,
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/store.rs is the module that writes durable files (DUR-1)"
    )]
    let opened = OpenOptions::new().write(true).create_new(true).open(path);
    let file = match opened {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => return Err(unwritable(error)),
    };
    // The umask may narrow the bits that `open` gives, so they are set
    // after the file exists.
    if let Err(error) = file.set_permissions(fs::Permissions::from_mode(mode)) {
        let _ = fs::remove_file(path);
        return Err(unwritable(error));
    }
    if let Some(dir) = path.parent() {
        sync_dir(dir);
    }
    Ok(true)
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
