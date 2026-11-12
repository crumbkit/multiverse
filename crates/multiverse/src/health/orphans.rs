//! The state that no record names, which
//! [`Workspaces::orphans`](crate::Workspaces::orphans) finds and
//! [`Workspaces::sweep_orphan`](crate::Workspaces::sweep_orphan) deletes:
//! a directory under the workspace root, the run files of a name and the
//! run record and logs of a workspace id, an archive that a repair moved
//! aside, a data directory of ignored files, and an archive directory
//! without a record.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use phase::WorkspaceId;

use crate::archive::{Archive, ArchiveState, Archives};
use crate::backend::Backend;
use crate::error::Error;
use crate::marker::MARKER_FILE;
use crate::name::WorkspaceName;
use crate::record::WorkspaceKind;
use crate::runs::Runs;
use crate::store;
use crate::workspaces::{ASIDE_SUFFIX, Workspaces, aside, delete_directory};

/// What [`Workspaces::orphans`] found.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct Orphans {
    /// Each item that no record names, in the order of the kinds of
    /// [`OrphanKind`], each kind in path order.
    pub items: Vec<Orphan>,
    /// The number of workspace and archive records that cannot be read.
    /// When it is more than 0, `items` is empty, because a record that
    /// cannot be read may name what would otherwise look orphaned.
    pub unreadable: usize,
}

/// One item that no record names.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Orphan {
    /// What the item is.
    pub kind: OrphanKind,
    /// Where it is: the directory, or the first of a run record's paths.
    pub path: PathBuf,
    /// The bytes that deleting it frees, or `None` when they were not
    /// measured. Only a moved-aside archive and a data directory are
    /// measured, because a person decides about each of them by its size.
    pub bytes: Option<u64>,
    /// One sentence for each thing that keeps the item, such as a file of
    /// a directory that no archive rebuilds. A sweep deletes only an item
    /// whose list is empty.
    pub kept: Vec<String>,
}

/// What an [`Orphan`] is. The enum is exhaustive, so a caller that prints
/// each kind must handle a new one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OrphanKind {
    /// A directory under the workspace root that no workspace record
    /// names, such as one that a restore left when it stopped during the
    /// rebuild.
    Directory {
        /// The workspace name that the directory has, or `None` when its
        /// name is no workspace name.
        name: Option<WorkspaceName>,
        /// The archive whose restore would write the directory again, or
        /// `None` when no archive would.
        archive: Option<WorkspaceName>,
    },
    /// The run files of a name that no workspace record holds, such as
    /// those that a removal left when it stopped after it deleted the
    /// record.
    RunFiles {
        /// The name.
        name: WorkspaceName,
    },
    /// phase's run record and logs of a workspace id that no workspace or
    /// archive record holds.
    RunRecord {
        /// The id.
        id: WorkspaceId,
        /// The run record and the directory of logs, those that exist.
        paths: Vec<PathBuf>,
    },
    /// An archive that `mxv repair NAME` moved aside to `.<name>.<id>`,
    /// because its pins were gone.
    MovedAside {
        /// The data directory of its ignored files, or `None` when it has
        /// none.
        data: Option<PathBuf>,
    },
    /// A data directory of ignored files whose archive id no record names,
    /// such as one that a restore left when it stopped after it deleted
    /// the record.
    IgnoredFiles,
    /// A directory of the archives that holds no record. An archive that
    /// is running makes its directory before it writes its record, so such
    /// a directory is only named.
    Unrecorded,
}

/// What [`Workspaces::sweep_orphan`] did with one item.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Swept {
    /// The item is deleted, with the docker volumes that these names name.
    Deleted {
        /// The names of the docker volumes that the sweep deleted.
        volumes: Vec<String>,
    },
    /// The item stays, for the reasons that each sentence gives.
    Kept(Vec<String>),
}

impl<B: Backend> Workspaces<B> {
    /// Returns each item that no record names: each directory under
    /// `root` without a workspace record, with what it holds that the
    /// archive of its name would not write again; the run files of each
    /// name and the run record of each id that no record holds; each
    /// archive that a repair moved aside and each data directory that no
    /// archive names, with their sizes; and each directory of the archives
    /// without a record. It changes nothing.
    ///
    /// A directory that no archive rebuilds may hold only an empty marker
    /// of its own. A directory whose name has an archive that has not
    /// finished is left out, because `mxv archive -w` finishes it.
    ///
    /// # Errors
    ///
    /// Returns the errors of reading the directories, the records and git.
    pub fn orphans(&self, archives: &Archives, runs: &Runs, root: &Path) -> Result<Orphans, Error> {
        // What may hold an orphan is read before the records. Each command
        // that makes such a thing writes its record first, except a
        // restore, which holds the lock of the name that a sweep takes.
        let leftovers = archives.leftovers()?;
        let ids = runs.phase_ids()?;
        let run_dirs = runs.run_dirs()?;
        let dirs = root_dirs(root)?;
        let records = self.store.list()?;
        let all = archives.list()?;
        let unreadable = records.problems.len() + all.problems.len();
        if unreadable > 0 {
            return Ok(Orphans {
                items: Vec::new(),
                unreadable,
            });
        }
        let mut items = Vec::new();
        for (path, target) in dirs {
            let named = records.workspaces.iter().any(|workspace| {
                workspace.path == path
                    || workspace.path == root.join(&target)
                    || workspace.name.as_str() == target
            });
            let unfinished = all.archives.iter().any(|archive| {
                archive.name.as_str() == target && archive.state == ArchiveState::Retiring
            });
            if named || unfinished {
                continue;
            }
            let name: Option<WorkspaceName> = target.parse().ok();
            let archive = all
                .archives
                .iter()
                .find(|archive| rebuilds(archive, &root.join(&target)));
            let kept = self.directory_kept(archives, archive, &path)?;
            items.push(Orphan {
                kind: OrphanKind::Directory {
                    name,
                    archive: archive.map(|archive| archive.name.clone()),
                },
                path,
                bytes: None,
                kept,
            });
        }
        for (name, path) in run_dirs {
            if records
                .workspaces
                .iter()
                .all(|workspace| workspace.name != name)
            {
                items.push(Orphan {
                    kind: OrphanKind::RunFiles { name },
                    path,
                    bytes: None,
                    kept: Vec::new(),
                });
            }
        }
        let aside_records: Vec<&Archive> = leftovers
            .aside
            .iter()
            .filter_map(|(_, record)| record.as_ref())
            .collect();
        let known: BTreeSet<&WorkspaceId> = records
            .workspaces
            .iter()
            .filter_map(|workspace| workspace.id.as_ref())
            .chain(
                all.archives
                    .iter()
                    .chain(aside_records.iter().copied())
                    .filter_map(|archive| archive.workspace_id.as_ref()),
            )
            .collect();
        for (id, paths) in ids {
            if known.contains(&id) {
                continue;
            }
            let Some(path) = paths.first().cloned() else {
                continue;
            };
            items.push(Orphan {
                kind: OrphanKind::RunRecord { id, paths },
                path,
                bytes: None,
                kept: Vec::new(),
            });
        }
        let mut aside_ids = BTreeSet::new();
        for (path, record) in &leftovers.aside {
            let id = record
                .as_ref()
                .map(|archive| archive.id.clone())
                .or_else(|| aside_id(path));
            let data = id
                .as_deref()
                .map(|id| archives.data_of(id))
                .filter(|data| data.is_dir());
            aside_ids.extend(id);
            let bytes = crate::disk::freeable(path)
                .saturating_add(data.as_deref().map_or(0, crate::disk::freeable));
            items.push(Orphan {
                kind: OrphanKind::MovedAside { data },
                path: path.clone(),
                bytes: Some(bytes),
                kept: Vec::new(),
            });
        }
        let live: BTreeSet<&str> = all
            .archives
            .iter()
            .map(|archive| archive.id.as_str())
            .collect();
        for (id, path) in &leftovers.data {
            if live.contains(id.as_str()) || aside_ids.contains(id) {
                continue;
            }
            items.push(Orphan {
                kind: OrphanKind::IgnoredFiles,
                path: path.clone(),
                bytes: Some(crate::disk::freeable(path)),
                kept: Vec::new(),
            });
        }
        for path in leftovers.unrecorded {
            items.push(Orphan {
                kind: OrphanKind::Unrecorded,
                path,
                bytes: None,
                kept: vec![
                    "no record names it, and an archive that is running writes its record there"
                        .to_owned(),
                ],
            });
        }
        Ok(Orphans {
            items,
            unreadable: 0,
        })
    }

    /// Deletes `orphan`, which [`Workspaces::orphans`] returned, after it
    /// checks again that no record names it. A directory is compared,
    /// moved aside to `.<name>.mxv-removing` beside it, compared again and
    /// deleted, and it moves back when the second comparison finds
    /// something. The run record of an id is swept by phase first, and the
    /// logs are deleted only when phase removed the record. The docker
    /// volumes labelled with an orphaned id are deleted only with
    /// `delete_volumes`. A directory of the archives without a record is
    /// never deleted. The caller holds the lock of the item's name, when
    /// it has one.
    ///
    /// # Errors
    ///
    /// Returns the errors of reading the records, of git, of phase, of
    /// docker, and of the moves and deletions.
    pub fn sweep_orphan(
        &self,
        archives: &Archives,
        runs: &Runs,
        orphan: &Orphan,
        delete_volumes: bool,
    ) -> Result<Swept, Error> {
        let deleted = || Swept::Deleted {
            volumes: Vec::new(),
        };
        match &orphan.kind {
            OrphanKind::Directory { archive, .. } => {
                self.sweep_directory(archives, &orphan.path, archive.as_ref())
            }
            OrphanKind::RunFiles { name } => {
                if self.store.read(name)?.is_some() {
                    return Ok(Swept::Kept(vec![format!(
                        "the workspace {name} has a record now"
                    )]));
                }
                runs.forget(name, None)?;
                Ok(deleted())
            }
            OrphanKind::RunRecord { id, .. } => {
                if self.knows(archives, id)? {
                    return Ok(Swept::Kept(vec![format!(
                        "a record holds the workspace id {id} now"
                    )]));
                }
                let held = runs.forget_id(id)?;
                if !held.is_empty() {
                    return Ok(Swept::Kept(held));
                }
                let mut volumes = Vec::new();
                if delete_volumes {
                    let found = runs.archived_volumes(id)?;
                    volumes = found.names();
                    runs.delete_volumes(found)?;
                }
                Ok(Swept::Deleted { volumes })
            }
            OrphanKind::MovedAside { data } => {
                Archives::delete_leftover(&orphan.path)?;
                if let Some(data) = data {
                    Archives::delete_leftover(data)?;
                }
                Ok(deleted())
            }
            OrphanKind::IgnoredFiles => {
                let id = orphan.path.file_name().and_then(|name| name.to_str());
                let named = archives
                    .list()?
                    .archives
                    .iter()
                    .any(|archive| Some(archive.id.as_str()) == id);
                if named {
                    return Ok(Swept::Kept(vec![
                        "an archive record names it now".to_owned(),
                    ]));
                }
                Archives::delete_leftover(&orphan.path)?;
                Ok(deleted())
            }
            OrphanKind::Unrecorded => Ok(Swept::Kept(orphan.kept.clone())),
        }
    }

    /// Reports whether a workspace record or an archive record, a moved
    /// aside one included, holds the workspace id `id`.
    fn knows(&self, archives: &Archives, id: &WorkspaceId) -> Result<bool, Error> {
        let holds = |archive: &Archive| archive.workspace_id.as_ref() == Some(id);
        Ok(self
            .store
            .list()?
            .workspaces
            .iter()
            .any(|workspace| workspace.id.as_ref() == Some(id))
            || archives.list()?.archives.iter().any(holds)
            || archives
                .leftovers()?
                .aside
                .iter()
                .any(|(_, record)| record.as_ref().is_some_and(holds)))
    }

    /// Returns what keeps the directory at `path`: what it holds that a
    /// restore of `archive` would not write again, or, without an archive,
    /// each entry other than an empty marker.
    fn directory_kept(
        &self,
        archives: &Archives,
        archive: Option<&Archive>,
        path: &Path,
    ) -> Result<Vec<String>, Error> {
        match archive {
            Some(archive) => self.rebuild_differences(archives, archive, path),
            None => own_entries(path),
        }
    }

    /// Deletes the directory at `path` that no workspace record names
    /// after the compare, the move aside and the compare again, as
    /// [`Workspaces::sweep_orphan`] says. `archive` is the name of the
    /// archive whose restore would write it again.
    fn sweep_directory(
        &self,
        archives: &Archives,
        path: &Path,
        archive: Option<&WorkspaceName>,
    ) -> Result<Swept, Error> {
        let Some(target) = target_of(path) else {
            return Ok(Swept::Kept(vec!["its path has no name".to_owned()]));
        };
        if self
            .store
            .list()?
            .workspaces
            .iter()
            .any(|workspace| workspace.path == target)
        {
            return Ok(Swept::Kept(vec![
                "a workspace record names it now".to_owned(),
            ]));
        }
        let record = match archive {
            Some(name) => match archives.read(name)? {
                Some(record) if rebuilds(&record, &target) => Some(record),
                _ => {
                    return Ok(Swept::Kept(vec![format!(
                        "the archive {name} changed since mxv compared the directory with it"
                    )]));
                }
            },
            None => None,
        };
        let kept = self.directory_kept(archives, record.as_ref(), path)?;
        if !kept.is_empty() {
            return Ok(Swept::Kept(kept));
        }
        let Some(doomed) = aside(&target) else {
            return Ok(Swept::Kept(vec!["its path has no name".to_owned()]));
        };
        if path != doomed {
            if doomed.symlink_metadata().is_ok() {
                return Ok(Swept::Kept(vec![format!(
                    "{} is in the way of the move aside",
                    doomed.display()
                )]));
            }
            store::move_aside(path, &doomed)?;
        }
        let kept = self.directory_kept(archives, record.as_ref(), &doomed)?;
        if !kept.is_empty() {
            // Something appeared between the two comparisons, so the
            // directory goes back, unless its place is taken by now.
            if target.symlink_metadata().is_err() {
                store::move_aside(&doomed, &target)?;
            }
            return Ok(Swept::Kept(kept));
        }
        delete_directory(&doomed)?;
        if let Some(record) = record.filter(|record| record.kind == WorkspaceKind::Worktree) {
            // The person's repository still lists each worktree that the
            // restore added, so the listing is pruned, as a removal does.
            for checkout in &record.checkouts {
                if checkout.source.is_dir() {
                    self.backend.prune_checkouts(&checkout.source)?;
                }
            }
        }
        Ok(Swept::Deleted {
            volumes: Vec::new(),
        })
    }
}

/// Reports whether a restore of `archive` writes the directory `target`:
/// the archive has finished and names that directory.
fn rebuilds(archive: &Archive, target: &Path) -> bool {
    archive.state == ArchiveState::Archived && archive.path == target
}

/// Returns the directory that the directory at `path` stands for: `path`
/// itself, or for `.<name>.mxv-removing`, where a sweep that stopped
/// moved it, the directory `<name>` beside it.
fn target_of(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    let target = name
        .strip_prefix('.')
        .and_then(|name| name.strip_suffix(ASIDE_SUFFIX))
        .unwrap_or(name);
    Some(path.with_file_name(target))
}

/// Returns each directory in `root` that may be a workspace's, with the
/// name of the directory it stands for: each directory whose name does
/// not start with `.`, and each `.<name>.mxv-removing` that a sweep that
/// stopped left. A symbolic link is never followed.
fn root_dirs(root: &Path) -> Result<Vec<(PathBuf, String)>, Error> {
    let unreadable = |source| Error::StateUnreadable {
        path: root.to_path_buf(),
        source,
    };
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(unreadable(error)),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(unreadable)?;
        if !entry.file_type().map_err(unreadable)?.is_dir() {
            continue;
        }
        let path = entry.path();
        let Some(target) = target_of(&path) else {
            continue;
        };
        let Some(name) = target.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') || (target != path && target.symlink_metadata().is_ok()) {
            continue;
        }
        found.push((path.clone(), name.to_owned()));
    }
    found.sort();
    Ok(found)
}

/// Returns one sentence for each entry of the directory at `path` other
/// than an empty marker that mxv wrote, which no archive writes again.
fn own_entries(path: &Path) -> Result<Vec<String>, Error> {
    let unreadable = |source| Error::StateUnreadable {
        path: path.to_path_buf(),
        source,
    };
    let mut found = Vec::new();
    for entry in fs::read_dir(path).map_err(unreadable)? {
        let entry = entry.map_err(unreadable)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == MARKER_FILE && crate::marker::is_own(&entry.path()) {
            continue;
        }
        found.push(format!("{name}, which no archive writes again"));
    }
    found.sort();
    Ok(found)
}

/// Returns the archive id of a directory `.<name>.<id>` that a repair
/// moved aside, read from its name.
fn aside_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let (_, id) = name.rsplit_once('.')?;
    Some(id.to_owned())
}
