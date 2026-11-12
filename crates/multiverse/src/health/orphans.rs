//! The state that no record names, which
//! [`Workspaces::orphans`](crate::Workspaces::orphans) finds and
//! [`Workspaces::sweep_orphan`](crate::Workspaces::sweep_orphan) deletes:
//! the run files of a name and phase's run record and logs of a workspace
//! id that no record holds, an archive that a repair moved aside, a data
//! directory of ignored files whose archive is gone, and a directory of the
//! archives without its own record.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use phase::WorkspaceId;

use crate::archive::{Archive, ArchiveRecords, ArchiveState, Archives};
use crate::backend::Backend;
use crate::error::Error;
use crate::name::WorkspaceName;
use crate::runs::Runs;
use crate::store;
use crate::workspaces::{Workspaces, delete_directory};

/// The reason that [`Workspaces::sweep_orphan`] gives for an item that a
/// fresh listing no longer holds as it was.
const CHANGED: &str = "another command changed it after mxv listed it";

/// The reason that [`Workspaces::sweep_orphan`] gives for a directory of
/// the archives without its own record, which it never deletes.
pub const UNRECORDED: &str = "mxv did not move it aside, or an archive that is running has not written its record yet, so mxv never deletes it";

/// One item that no record names.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Orphan {
    /// What the item is.
    pub kind: OrphanKind,
    /// Where it is: the directory, or the first of a run record's paths.
    pub path: PathBuf,
    /// The bytes that deleting it frees, so that a person decides about it
    /// by its size. The fresh listing that a sweep makes leaves it 0.
    pub bytes: u64,
}

/// What an [`Orphan`] is. The enum is exhaustive, so a caller that prints
/// each kind must handle a new one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OrphanKind {
    /// The run files of a name that no workspace record holds, such as
    /// those that a removal left when it stopped after it deleted the
    /// record. They are mxv's own derived state.
    RunFiles {
        /// The name.
        name: WorkspaceName,
    },
    /// phase's run record and logs of a workspace id that no workspace or
    /// archive record holds. phase's sweep stops what the record names
    /// before the record and the logs are deleted.
    RunRecord {
        /// The id.
        id: WorkspaceId,
        /// The run record and the directory of logs, those that exist.
        paths: Vec<PathBuf>,
    },
    /// An archive that `mxv repair NAME` moved aside to `.<name>.<id>`,
    /// because its pins were gone. It holds the only copies of the
    /// workspace's root files.
    MovedAside {
        /// The data directory of its ignored files, or `None` when it has
        /// none.
        data: Option<PathBuf>,
    },
    /// A data directory of ignored files whose archive id no record names,
    /// such as the one of a moved-aside archive whose directory a person
    /// deleted by hand. It holds the only copies of those files.
    IgnoredFiles,
    /// A directory of the archives without its own record: a hidden one
    /// that holds no record or another archive's, or one of a name that
    /// holds no record. An archive that is running makes its directory
    /// before it writes its record, so such a directory is only named and
    /// never deleted.
    Unrecorded,
}

impl OrphanKind {
    /// Reports whether `self` at `path` and `other` at `other_path` are the
    /// same item. A run record is the same while it has the same id,
    /// whichever of its paths still exist.
    fn same(&self, path: &Path, other: &Self, other_path: &Path) -> bool {
        match (self, other) {
            (Self::RunRecord { id, .. }, Self::RunRecord { id: other, .. }) => id == other,
            _ => self == other && path == other_path,
        }
    }
}

/// What [`Workspaces::sweep_orphan`] did with one item. The enum is
/// exhaustive, so a caller that prints each outcome must handle a new one.
#[derive(Debug)]
pub enum Swept {
    /// The item is deleted, with the docker volumes that these names name.
    Deleted {
        /// The names of the docker volumes that the sweep deleted.
        volumes: Vec<String>,
    },
    /// The item stays, for the reasons that each sentence gives, such as
    /// that a fresh listing no longer holds it as it was.
    Kept(Vec<String>),
    /// The sweep began to change the item and failed, so part of it may be
    /// gone.
    PartlyDeleted {
        /// The item's paths that are gone.
        deleted: Vec<PathBuf>,
        /// Why the sweep stopped.
        error: Error,
    },
}

impl<B: Backend> Workspaces<B> {
    /// Returns each item that no record names, in the order of the kinds
    /// of [`OrphanKind`], each kind in path order, with their sizes: the
    /// run files of each name and phase's run record of each id that no
    /// record holds, each archive that a repair moved aside and each data
    /// directory that no archive names, and each directory of the archives
    /// without its own record. It writes nothing and takes no lock.
    ///
    /// The run files of a name whose archive has not finished are left
    /// out, because `mxv archive -w` deletes them.
    ///
    /// # Errors
    ///
    /// Returns the read error of the first workspace or archive record that
    /// cannot be read, a moved-aside one included, because such a record
    /// may name what would otherwise look orphaned, and the errors of
    /// reading the directories.
    pub fn orphans(&self, archives: &Archives, runs: &Runs) -> Result<Vec<Orphan>, Error> {
        self.scan(archives, runs, true)
    }

    /// Returns what [`Workspaces::orphans`] returns, with each size
    /// measured only when `sized`.
    fn scan(&self, archives: &Archives, runs: &Runs, sized: bool) -> Result<Vec<Orphan>, Error> {
        // What may hold an orphan is read before the records. Each command
        // that makes such a thing writes its record first, except one that
        // holds the lock of the name, which a sweep of run files takes.
        let leftovers = archives.leftovers()?;
        let ids = runs.phase_ids()?;
        let run_dirs = runs.run_dirs()?;
        let workspaces = self.store.list()?;
        if let Some(problem) = workspaces.problems.into_iter().next() {
            return Err(problem);
        }
        let workspaces = workspaces.workspaces;
        let records = readable(archives.list()?)?;
        let size = |path: &Path| {
            if sized {
                crate::disk::freeable(path)
            } else {
                0
            }
        };
        let mut items = Vec::new();
        for (name, path) in run_dirs {
            let named = workspaces.iter().any(|workspace| workspace.name == name)
                || records
                    .iter()
                    .any(|archive| archive.name == name && archive.state == ArchiveState::Retiring);
            if !named {
                items.push(Orphan {
                    bytes: size(&path),
                    kind: OrphanKind::RunFiles { name },
                    path,
                });
            }
        }
        let known: BTreeSet<&WorkspaceId> = workspaces
            .iter()
            .filter_map(|workspace| workspace.id.as_ref())
            .chain(
                records
                    .iter()
                    .chain(leftovers.aside.iter().map(|(_, record)| record))
                    .filter_map(|archive| archive.workspace_id.as_ref()),
            )
            .collect();
        for id in ids {
            if known.contains(&id) {
                continue;
            }
            let paths = runs.phase_paths(&id);
            let Some(path) = paths.first().cloned() else {
                continue;
            };
            let bytes = paths.iter().map(|path| size(path)).sum();
            items.push(Orphan {
                kind: OrphanKind::RunRecord { id, paths },
                path,
                bytes,
            });
        }
        let mut aside_ids = BTreeSet::new();
        for (path, record) in &leftovers.aside {
            let data = Some(archives.data_of(&record.id)).filter(|data| data.is_dir());
            aside_ids.insert(record.id.as_str());
            let bytes = size(path).saturating_add(data.as_deref().map_or(0, size));
            items.push(Orphan {
                kind: OrphanKind::MovedAside { data },
                path: path.clone(),
                bytes,
            });
        }
        let live: BTreeSet<&str> = records.iter().map(|archive| archive.id.as_str()).collect();
        for (id, path) in &leftovers.data {
            if live.contains(id.as_str()) || aside_ids.contains(id.as_str()) {
                continue;
            }
            items.push(Orphan {
                kind: OrphanKind::IgnoredFiles,
                path: path.clone(),
                bytes: size(path),
            });
        }
        for path in leftovers.unrecorded {
            items.push(Orphan {
                kind: OrphanKind::Unrecorded,
                bytes: size(&path),
                path,
            });
        }
        Ok(items)
    }

    /// Deletes `orphan`, which [`Workspaces::orphans`] returned, when a
    /// fresh listing still holds it, and takes the lock of the name of run
    /// files first. Run files are deleted. phase's sweep stops what a run
    /// record names, and the record's logs are deleted only when phase
    /// removed the record; with `delete_volumes`, the docker volumes that
    /// docker labels with its id are deleted too. A moved-aside archive is
    /// deleted and then its data directory, and a data directory is
    /// deleted. A directory of the archives without its own record is never
    /// deleted.
    ///
    /// # Errors
    ///
    /// Returns the errors of the lock and of the fresh listing, which come
    /// before anything of the item changes. A failure after that returns
    /// [`Swept::PartlyDeleted`].
    pub fn sweep_orphan(
        &self,
        archives: &Archives,
        runs: &Runs,
        orphan: &Orphan,
        delete_volumes: bool,
    ) -> Result<Swept, Error> {
        let _lock = match &orphan.kind {
            OrphanKind::RunFiles { name } => Some(runs.lock(name)?),
            _ => None,
        };
        let current = self
            .scan(archives, runs, false)?
            .into_iter()
            .any(|item| orphan.kind.same(&orphan.path, &item.kind, &item.path));
        if !current {
            return Ok(Swept::Kept(vec![CHANGED.to_owned()]));
        }
        let paths = paths_of(orphan);
        let swept = match &orphan.kind {
            OrphanKind::RunFiles { name } => runs.forget(name, None).map(|()| Vec::new()),
            OrphanKind::RunRecord { id, .. } => match runs.forget_id(id) {
                Ok(held) if !held.is_empty() => return Ok(Swept::Kept(held)),
                Ok(_) if delete_volumes => runs.archived_volumes(id).and_then(|found| {
                    let names = found.names();
                    runs.delete_volumes(found).map(|()| names)
                }),
                Ok(_) => Ok(Vec::new()),
                Err(error) => Err(error),
            },
            OrphanKind::MovedAside { .. } | OrphanKind::IgnoredFiles => paths
                .iter()
                .try_for_each(|path| delete(path))
                .map(|()| Vec::new()),
            OrphanKind::Unrecorded => return Ok(Swept::Kept(vec![UNRECORDED.to_owned()])),
        };
        Ok(match swept {
            Ok(volumes) => Swept::Deleted { volumes },
            Err(error) => Swept::PartlyDeleted {
                deleted: paths
                    .into_iter()
                    .filter(|path| fs::symlink_metadata(path).is_err())
                    .collect(),
                error,
            },
        })
    }
}

/// Returns the paths that a sweep of `orphan` deletes, in the order that
/// it deletes them.
fn paths_of(orphan: &Orphan) -> Vec<PathBuf> {
    match &orphan.kind {
        OrphanKind::RunRecord { paths, .. } => paths.clone(),
        OrphanKind::MovedAside { data } => std::iter::once(orphan.path.clone())
            .chain(data.clone())
            .collect(),
        OrphanKind::RunFiles { .. } | OrphanKind::IgnoredFiles | OrphanKind::Unrecorded => {
            vec![orphan.path.clone()]
        }
    }
}

/// Returns the archives of `records`, or the first error of a record that
/// cannot be read.
fn readable(records: ArchiveRecords) -> Result<Vec<Archive>, Error> {
    match records.problems.into_iter().next() {
        Some(problem) => Err(problem),
        None => Ok(records.archives),
    }
}

/// Deletes the directory `path` by file descriptor and syncs its parent.
fn delete(path: &Path) -> Result<(), Error> {
    delete_directory(path)?;
    if let Some(parent) = path.parent() {
        store::sync_dir(parent);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
