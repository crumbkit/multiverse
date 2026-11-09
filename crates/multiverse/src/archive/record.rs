//! The archive record and the directory of archives that holds it.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read as _};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use phase::WorkspaceId;
use serde::{Deserialize, Serialize};

use crate::archive::root::RootEntry;
use crate::archive::{ARCHIVE_VERSION, PIN_NAMESPACE, RECORD_FILE};
use crate::error::Error;
use crate::name::WorkspaceName;
use crate::record::{Checkout, Workspace, WorkspaceKind};
use crate::store::{self, MOST_RECORD_BYTES};
use crate::workspaces::delete_directory;

/// Whether an archive has finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArchiveState {
    /// The record is written and the workspace is still being retired. The
    /// same `mxv archive` finishes it.
    Retiring,
    /// The workspace is gone, and `mxv restore` can rebuild it.
    Archived,
}

/// One checkout of an archived workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ArchivedCheckout {
    /// The checkout's folder in the workspace's directory.
    pub folder: String,
    /// The checkout's directory.
    pub path: PathBuf,
    /// The top-level directory of the person's repository.
    pub source: PathBuf,
    /// The URL of the checkout's `origin`, without a password, and without
    /// a user name unless it is an SSH URL, or `None` when it has no origin.
    pub origin: Option<String>,
    /// The branch that the checkout had checked out, or the branch that it
    /// was created on when `detached` is set.
    pub branch: String,
    /// Whether the checkout's `HEAD` was detached from every branch.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detached: bool,
    /// The remote branch that the workspace's branch started from, such as
    /// `refs/remotes/origin/main`.
    pub base: String,
    /// The branch on the remote that the checkout's branch tracked, such as
    /// `refs/heads/triage`, or `None` when it tracked none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// The remote that `upstream` lies on, or `None` when the branch tracked
    /// none or the record is older than this field, which means `origin`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_remote: Option<String>,
    /// The commit that the checkout's `HEAD` named.
    pub commit: String,
    /// The bare mirror that holds a clone's pin, or `None` for a worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror: Option<PathBuf>,
    /// The reference that keeps the commit.
    pub pin: String,
    /// The commits of `HEAD` that no remote branch of `origin` held when the
    /// workspace was archived, which exist only on this machine.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_only: Vec<String>,
}

impl ArchivedCheckout {
    /// Returns the repository that holds the checkout's pin: its mirror, or
    /// its source for a worktree.
    #[must_use]
    pub fn pin_repository(&self) -> &Path {
        self.mirror.as_deref().unwrap_or(&self.source)
    }
}

/// The record of one archive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Archive {
    /// The version of the record form, which is [`ARCHIVE_VERSION`].
    pub version: u32,
    /// The archive's own id, which names its pins: 32 lowercase hex digits.
    pub id: String,
    /// Whether the archive has finished.
    pub state: ArchiveState,
    /// When the archive started, in seconds since the Unix epoch.
    pub time: u64,
    /// The workspace's name.
    pub name: WorkspaceName,
    /// The workspace's stable id, which its docker volumes are labelled
    /// with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// How the workspace's checkouts were made.
    pub kind: WorkspaceKind,
    /// The workspace's directory.
    pub path: PathBuf,
    /// The host port that the workspace held for each named port.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ports: BTreeMap<String, u16>,
    /// Whether `mxv pin` kept `mxv reclaim` from stopping the workspace.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    /// Whether any of the workspace's containers has run, so docker may
    /// hold volumes labelled with its stable id.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub containers_ran: bool,
    /// The workspace's checkouts, in the record's order.
    pub checkouts: Vec<ArchivedCheckout>,
    /// Each thing that `--force` lost, with the checkout's folder in front.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lost: Vec<String>,
    /// The extra entries of the workspace's directory, parents before their
    /// children.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub root: Vec<RootEntry>,
    /// The SHA-256 digest of the workspace manifest's bytes, or `None` when
    /// the workspace has no workspace manifest of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
    /// The permission bits of the workspace manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_mode: Option<u32>,
}

impl Archive {
    /// Returns the archive's checkouts as the checkouts of a workspace, each
    /// on its recorded branch, with no commit yet.
    pub(crate) fn workspace_checkouts(&self) -> Vec<Checkout> {
        self.checkouts
            .iter()
            .map(|archived| {
                let mut checkout = Checkout::new(
                    archived.folder.clone(),
                    &archived.source,
                    archived.path.clone(),
                    archived.branch.clone(),
                    archived.base.clone(),
                );
                checkout.mirror.clone_from(&archived.mirror);
                checkout
            })
            .collect()
    }

    /// Returns the record of the workspace that the archive holds, with its
    /// stable id, ports and checkouts.
    #[must_use]
    pub fn workspace(&self) -> Workspace {
        let mut workspace = Workspace::new(
            self.name.clone(),
            self.path.clone(),
            self.workspace_checkouts(),
        );
        workspace.id.clone_from(&self.workspace_id);
        if self.kind == WorkspaceKind::Worktree {
            workspace = workspace.of_worktrees();
        }
        workspace.ports.clone_from(&self.ports);
        workspace.pinned = self.pinned;
        workspace
    }

    /// Reports whether a checkout's commit exists on no remote.
    #[must_use]
    pub fn has_local_only_commits(&self) -> bool {
        self.checkouts
            .iter()
            .any(|checkout| !checkout.local_only.is_empty())
    }
}

/// The part of a record that is read first, so a record of a newer version
/// is refused before its other fields are read.
#[derive(Deserialize)]
struct Versioned {
    /// The record's version.
    version: u32,
}

/// The archives that one state directory holds.
#[derive(Clone, Debug)]
pub struct Archives {
    /// The directory that holds one directory for each archive.
    dir: PathBuf,
}

/// The archives that [`Archives::list`] found.
#[derive(Debug)]
#[non_exhaustive]
pub struct ArchiveRecords {
    /// Each archive that could be read, in name order.
    pub archives: Vec<Archive>,
    /// The error of each record that could not be read, in name order.
    pub problems: Vec<Error>,
}

impl Archives {
    /// Returns the archives in `dir`, such as
    /// `~/.local/state/mxv/archives`.
    #[must_use]
    pub const fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Returns the directory of the archive of `name`.
    #[must_use]
    pub fn dir_of(&self, name: &WorkspaceName) -> PathBuf {
        self.dir.join(name.as_str())
    }

    /// Returns the record of the archive of `name`, or `None` when no
    /// archive has the name.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StateUnreadable`] when the record cannot be read,
    /// [`Error::NewerRecord`] when a newer multiverse wrote it,
    /// [`Error::CorruptRecord`] when it is not a record, and
    /// [`Error::MismatchedRecord`] when it holds another name.
    pub fn read(&self, name: &WorkspaceName) -> Result<Option<Archive>, Error> {
        let path = self.dir_of(name).join(RECORD_FILE);
        let found = read_record(&path)?;
        match found {
            Some(archive) if archive.name != *name => Err(Error::MismatchedRecord {
                path,
                name: archive.name.to_string(),
            }),
            found => Ok(found),
        }
    }

    /// Returns every archive in name order, and the error of each record
    /// that cannot be read, so one bad record does not hide the others.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StateUnreadable`] when the directory cannot be read.
    pub fn list(&self) -> Result<ArchiveRecords, Error> {
        let unreadable = |source| Error::StateUnreadable {
            path: self.dir.clone(),
            source,
        };
        let mut records = ArchiveRecords {
            archives: Vec::new(),
            problems: Vec::new(),
        };
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(records),
            Err(error) => return Err(unreadable(error)),
        };
        let mut paths = Vec::new();
        for entry in entries {
            paths.push(entry.map_err(unreadable)?.path().join(RECORD_FILE));
        }
        paths.sort();
        for path in paths {
            match read_record(&path) {
                Ok(Some(archive)) => records.archives.push(archive),
                Ok(None) => {}
                Err(error) => records.problems.push(error),
            }
        }
        Ok(records)
    }

    /// Writes the record of `archive`, and syncs it and its directory before
    /// it returns. The archive's directory is private to the person.
    pub(super) fn write(&self, archive: &Archive) -> Result<(), Error> {
        let dir = self.dir_of(&archive.name);
        store::private_dir(&dir)?;
        let target = dir.join(RECORD_FILE);
        let mut bytes =
            serde_json::to_vec_pretty(archive).map_err(|source| Error::CorruptRecord {
                path: target.clone(),
                source,
            })?;
        bytes.push(b'\n');
        if bytes.len() as u64 > MOST_RECORD_BYTES {
            return Err(Error::Unwritable {
                path: target,
                source: io::ErrorKind::FileTooLarge.into(),
            });
        }
        store::replace_file(&target, &bytes)
    }

    /// Deletes the directory of the archive of `name` and everything in it.
    pub(super) fn delete(&self, name: &WorkspaceName) -> Result<(), Error> {
        delete_directory(&self.dir_of(name))?;
        store::sync_dir(&self.dir);
        Ok(())
    }
}

/// Returns the archive record at `path`, or `None` when no file is there.
fn read_record(path: &Path) -> Result<Option<Archive>, Error> {
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
    if versioned.version > ARCHIVE_VERSION {
        return Err(Error::NewerRecord {
            path: path.to_path_buf(),
            version: versioned.version,
        });
    }
    let archive: Archive = serde_json::from_slice(&bytes).map_err(corrupt)?;
    if let Some(entry) = archive.root.iter().find(|entry| !stays_inside(&entry.path)) {
        return Err(corrupt(serde::de::Error::custom(format!(
            "the root entry {} leaves the workspace's directory",
            entry.path
        ))));
    }
    for checkout in &archive.checkouts {
        if !inside_workspace(&archive.path, &checkout.path) {
            return Err(corrupt(serde::de::Error::custom(format!(
                "the checkout {} leaves the workspace's directory",
                checkout.folder
            ))));
        }
        if !is_pin(&checkout.pin) {
            return Err(corrupt(serde::de::Error::custom(format!(
                "the pin {} leaves {PIN_NAMESPACE}",
                checkout.pin
            ))));
        }
    }
    Ok(Some(archive))
}

/// Reports whether `path`, a checkout's path from a record, is the
/// workspace's directory `workspace` or lies below it. An old workspace's
/// checkout can be the directory itself.
fn inside_workspace(workspace: &Path, path: &Path) -> bool {
    path.strip_prefix(workspace).is_ok_and(|below| {
        below
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    })
}

/// Reports whether `pin`, a reference from a record, lies below the
/// namespace of the archives' pins.
fn is_pin(pin: &str) -> bool {
    pin.strip_prefix(PIN_NAMESPACE).is_some_and(stays_inside)
}

/// Reports whether `path`, a path from a record, names something below the
/// directory that it is joined to.
fn stays_inside(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

/// Returns the seconds since the Unix epoch.
pub(super) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
