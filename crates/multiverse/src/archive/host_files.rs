//! The files that a host keeps for a workspace outside its checkouts, such
//! as test evidence, which an archive carries through its [`ArchiveHook`].
//!
//! During an archive, after the root files are copied and before the first
//! checkout moves, the hook's [`ArchiveHook::save`] writes the host's files
//! into `host.partial/` in the archive's data directory. The archive refuses
//! a link, a socket, a device and a name that is not UTF-8, checks the
//! limits [`MOST_HOST_FILES`], [`ARCHIVE_MAX_FILE`] and
//! [`MOST_HOST_FILE_BYTES`], records each file as a [`HostFile`] with its
//! SHA-256 digest, syncs the files and renames the directory to `host/`.
//!
//! During a restore, after the marker and before the record, the restore
//! copies each file into the workspace's directory below
//! [`Host::host_dir`](crate::Host::host_dir), checks its digest, and calls
//! [`ArchiveHook::check`]. A damaged file or a failed check undoes the
//! restore and keeps the archive.
//!
//! The files are plain files with the permission bits that the hook gave
//! them, in the caller's data directory and in the workspace's directory,
//! so a host must not keep a secret in them.

use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::archive::{ARCHIVE_MAX_FILE, Archive, Archives, RestoreRefusal};
use crate::error::Error;
use crate::record::Workspace;
use crate::store;
use crate::workspaces::delete_directory;

/// The most files that a host's hook may write for one archive. The
/// archive stops with [`Error::HostFilesTooMany`] before any checkout
/// moves; the host then carries fewer files, such as one archive file that
/// it makes itself.
pub const MOST_HOST_FILES: usize = 1_000;

/// The most bytes of all the files that a host's hook may write for one
/// archive: 1 GiB. The archive stops with [`Error::HostFilesTooLarge`]
/// before any checkout moves.
pub const MOST_HOST_FILE_BYTES: u64 = 1024 * 1024 * 1024;

/// The directory in an archive's data directory that the hook writes into.
const PARTIAL_DIR: &str = "host.partial";

/// The directory in an archive's data directory that holds the host's
/// files once the archive has checked and listed them.
const KEPT_DIR: &str = "host";

/// Why a host's hook failed, in the host's own words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookError {
    /// The message.
    message: String,
}

impl HookError {
    /// Returns the failure with `message`.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Returns the message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for HookError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HookError {}

/// What a host gives [`Archives::with_archive_hook`] so that an archive
/// carries the files that the host keeps for a workspace outside its
/// checkouts, and a restore puts them back.
pub trait ArchiveHook: Send + Sync {
    /// Writes the host's files for `workspace` into `into`, an empty
    /// directory with the permission bits 0700 that multiverse made inside
    /// the archive. Only regular files and directories may be written.
    ///
    /// # Errors
    ///
    /// Returns why the files could not be written, which stops the archive
    /// before any checkout moves.
    fn save(&self, workspace: &Workspace, into: &Path) -> Result<(), HookError>;

    /// Checks the files that a restore put back at `restored`, before the
    /// workspace's record is written as live.
    ///
    /// # Errors
    ///
    /// Returns why the files are not right, which undoes the restore and
    /// keeps the archive.
    fn check(&self, workspace: &Workspace, restored: &Path) -> Result<(), HookError>;
}

/// The hook that [`Archives`] holds, which prints as its name only.
#[derive(Clone)]
pub(crate) struct Hook(pub(crate) Arc<dyn ArchiveHook>);

impl fmt::Debug for Hook {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ArchiveHook")
    }
}

/// One file that a host's hook wrote, as the archive record lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HostFile {
    /// The file's path, relative to the directory that the hook wrote
    /// into.
    pub path: String,
    /// Its size in bytes.
    pub size: u64,
    /// Its permission bits.
    pub mode: u32,
    /// The SHA-256 digest of its bytes, in lowercase hex.
    pub digest: String,
}

/// Runs the hook of `archives`, when it has one, for the workspace of
/// `archive`, and lists the files that it wrote in `archive`. The caller
/// writes the record afterwards. A run that an earlier one left partway,
/// with `host.partial/` or with `host/` that the record does not list, is
/// redone from the start.
pub(super) fn keep(
    archives: &Archives,
    archive: &mut Archive,
    workspace: &Workspace,
) -> Result<(), Error> {
    let Some(hook) = archives.hook() else {
        return Ok(());
    };
    let data = archives.data_of(&archive.id);
    let partial = data.join(PARTIAL_DIR);
    let kept = data.join(KEPT_DIR);
    if !archive.host_files.is_empty() {
        // The record stops listing the files before they go, so it never
        // names files that are gone.
        archive.host_files.clear();
        archives.write(archive)?;
    }
    delete_directory(&partial)?;
    delete_directory(&kept)?;
    store::private_dir(&data)?;
    store::private_dir(&partial)?;
    hook.0
        .save(workspace, &partial)
        .map_err(|error| Error::HostHookFailed {
            name: archive.name.clone(),
            message: error.message,
        })?;
    let mut walk = Walk {
        name: archive.name.clone(),
        files: Vec::new(),
        bytes: 0,
    };
    walk.dir(&partial, "")?;
    walk.files.sort_by(|left, right| left.path.cmp(&right.path));
    store::move_aside(&partial, &kept)?;
    store::sync_dir(&data);
    archive.host_files = walk.files;
    Ok(())
}

/// The files that [`keep`] found so far.
struct Walk {
    /// The archive's name, which the errors name.
    name: crate::name::WorkspaceName,
    /// Each file.
    files: Vec<HostFile>,
    /// Their bytes.
    bytes: u64,
}

impl Walk {
    /// Adds each file below `dir`, whose path relative to the hook's
    /// directory is `prefix`, after it checks its kind and the limits, and
    /// syncs it and `dir`.
    fn dir(&mut self, dir: &Path, prefix: &str) -> Result<(), Error> {
        let unreadable = |source| Error::StateUnreadable {
            path: dir.to_path_buf(),
            source,
        };
        for entry in fs::read_dir(dir).map_err(unreadable)? {
            let entry = entry.map_err(unreadable)?;
            let path = entry.path();
            let shown = format!("{prefix}{}", entry.file_name().to_string_lossy());
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                return Err(self.unkeepable(shown));
            };
            let relative = format!("{prefix}{name}");
            let metadata = fs::symlink_metadata(&path).map_err(unreadable)?;
            if metadata.is_dir() {
                self.dir(&path, &format!("{relative}/"))?;
                continue;
            }
            if !metadata.is_file() {
                return Err(self.unkeepable(relative));
            }
            if self.files.len() >= MOST_HOST_FILES {
                return Err(Error::HostFilesTooMany {
                    name: self.name.clone(),
                    count: self.files.len() + 1,
                });
            }
            let size = metadata.len();
            if size > ARCHIVE_MAX_FILE {
                return Err(Error::HostFileTooLarge {
                    name: self.name.clone(),
                    path: relative,
                    bytes: size,
                });
            }
            self.bytes = self.bytes.saturating_add(size);
            if self.bytes > MOST_HOST_FILE_BYTES {
                return Err(Error::HostFilesTooLarge {
                    name: self.name.clone(),
                    bytes: self.bytes,
                });
            }
            sync_file(&path)?;
            self.files.push(HostFile {
                path: relative,
                size,
                mode: metadata.permissions().mode() & 0o7777,
                digest: store::file_digest(&path)?,
            });
        }
        store::sync_dir(dir);
        Ok(())
    }

    /// Returns the refusal of the entry `path`.
    fn unkeepable(&self, path: String) -> Error {
        Error::HostFileUnkeepable {
            name: self.name.clone(),
            path,
        }
    }
}

/// Syncs the file at `path`, so its bytes survive a power loss before the
/// record lists it.
fn sync_file(path: &Path) -> Result<(), Error> {
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|source| Error::Unwritable {
            path: path.to_path_buf(),
            source,
        })
}

/// Copies the host files of `archive` into `into`, which it makes with the
/// permission bits 0700, checking each file against its digest, and then
/// runs the hook's check of `workspace`. It returns the refusal when a file
/// is missing or damaged, or the check fails. The caller marks `into` as
/// the run's before it calls this, and deletes it after a refusal.
pub(super) fn put_back(
    archives: &Archives,
    archive: &Archive,
    workspace: &Workspace,
    into: &Path,
) -> Result<Result<(), RestoreRefusal>, Error> {
    let Some(hook) = archives.hook() else {
        return Ok(Ok(()));
    };
    let kept = archives.data_of(&archive.id).join(KEPT_DIR);
    store::private_dir(into)?;
    let mut damaged = Vec::new();
    for file in &archive.host_files {
        let to = into.join(&file.path);
        if let Some(parent) = to.parent() {
            make_dirs(into, parent)?;
        }
        match store::copy_file(&kept.join(&file.path), &to, file.mode) {
            Ok(digest) if digest == file.digest => {}
            Ok(_) => damaged.push(file.path.clone()),
            Err(Error::StateUnreadable { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                damaged.push(file.path.clone());
            }
            Err(error) => return Err(error),
        }
    }
    if !damaged.is_empty() {
        return Ok(Err(RestoreRefusal::HostFilesDamaged { paths: damaged }));
    }
    store::sync_dir(into);
    Ok(hook
        .0
        .check(workspace, into)
        .map_err(|error| RestoreRefusal::HostCheckFailed {
            message: error.message,
        }))
}

/// Makes `dir` and each directory between `root` and it with the
/// permission bits 0700.
fn make_dirs(root: &Path, dir: &Path) -> Result<(), Error> {
    let mut path = PathBuf::from(root);
    let below = dir.strip_prefix(root).unwrap_or(dir);
    for part in below.components() {
        path.push(part);
        if !path.is_dir() {
            store::private_dir(&path)?;
        }
    }
    Ok(())
}
