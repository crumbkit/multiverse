//! The health of the records that multiverse keeps.
//!
//! [`classify`] compares each workspace record and each archive record with
//! the state on disk that it should match, and returns a [`Broken`] for
//! each one that does not, with the [`Repair`] that brings it back. It
//! stores nothing and changes nothing, so a listing can call it on every
//! run. [`Workspaces::repair`](crate::Workspaces::repair) performs the
//! repairs that [`Repair::Repair`] names, and
//! [`Workspaces::orphans`](crate::Workspaces::orphans) finds what no record
//! names.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use crate::archive::{Archive, ArchiveRecords, ArchiveState};
use crate::backend::Backend;
use crate::error::Error;
use crate::name::WorkspaceName;
use crate::record::Workspace;
use crate::store::Records;

mod orphans;
mod repair;

pub use orphans::{Orphan, OrphanKind, Swept, UNRECORDED};
pub use repair::Repaired;

#[cfg(test)]
mod tests;

/// The broken records that [`classify`] found, by the listing that shows
/// them.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct Health {
    /// The broken records that a listing of the workspaces shows: each
    /// workspace record that cannot be read or does not match its
    /// directory, each archive that has not finished, and each archive
    /// that a restore left beside the workspace it rebuilt. The records
    /// that cannot be read come first, and then the others in name order.
    pub workspaces: Vec<Broken>,
    /// The broken records that a listing of the archives shows: each
    /// archive record that cannot be read, that has not finished, or that
    /// a restore left behind, in the same order.
    pub archives: Vec<Broken>,
}

/// One record that does not match the state it should be in.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Broken {
    /// The workspace's name, or the name of the record that cannot be
    /// read: a workspace record's file name without `.json`, or an archive
    /// record's directory.
    pub name: String,
    /// What is wrong.
    pub problem: Problem,
    /// What brings the record back, or `None` when no command can.
    pub repair: Option<Repair>,
}

/// What is wrong with a [`Broken`] record. The enum is exhaustive, so a
/// caller that prints each problem must handle a new one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// The record cannot be read, such as a record that is not JSON or
    /// that a newer multiverse wrote.
    Unreadable {
        /// The error's text, which names the record's file.
        reason: String,
    },
    /// The archive is still [`ArchiveState::Retiring`], and either no
    /// workspace has its name and its pins are there, or the workspace of
    /// that name is the one it archives.
    ArchiveUnfinished,
    /// The archive is still [`ArchiveState::Retiring`], no workspace has
    /// its name, and the pin of no checkout names its commit, so finishing
    /// it would claim that a restore brings back what is gone.
    ArchivePinsGone,
    /// The archive is still [`ArchiveState::Retiring`], and a workspace
    /// with another stable id has its name, so archiving the name again
    /// refuses.
    ArchiveBesideNewerWorkspace,
    /// A repair stopped while it moved an older archive to its dated name,
    /// so this record and another one hold the same archive id, and the
    /// archive's files are split between their two directories. Both rows
    /// name the repair of the older name, and a repair of either name
    /// finishes the move.
    RenameUnfinished,
    /// The archive is [`ArchiveState::Archived`], and the workspace of its
    /// name has its stable id with its directory and every checkout's
    /// directory there, as [`Rebuilt::Whole`] says, so a restore rebuilt
    /// the workspace and stopped before it deleted the archive.
    RestoreUnfinished,
    /// The workspace has checkouts, and every checkout's directory is gone,
    /// while the directory that holds the workspace's directory is there.
    CheckoutsGone,
}

/// What brings a [`Broken`] record back. The enum is exhaustive, so a
/// caller that prints each repair must handle a new one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Repair {
    /// Archiving the workspace of this name again, which finishes the
    /// archive from the state it left.
    Archive(WorkspaceName),
    /// Removing the workspace of this name, which deletes its record and
    /// what is left of it.
    Remove(WorkspaceName),
    /// Repairing the records of this name with
    /// [`Workspaces::repair`](crate::Workspaces::repair).
    Repair(WorkspaceName),
    /// Moving the record file at this path aside by hand. multiverse never
    /// deletes a record that it cannot read.
    MoveAside(PathBuf),
}

/// Returns each record of `records` and `archives` that does not match the
/// state it should be in, with its repair.
///
/// It reads whether each checkout's directory exists, and, through
/// `backend`, whether the pins of each archive that has not finished and
/// whose workspace is gone are still there. A pin that cannot be read
/// counts as there, so the repair stays `mxv archive -w`.
#[must_use]
pub fn classify<B: Backend>(backend: &B, records: &Records, archives: &ArchiveRecords) -> Health {
    let mut health = Health {
        workspaces: records
            .problems
            .iter()
            .map(|error| unreadable(error, Path::file_stem))
            .collect(),
        archives: archives
            .problems
            .iter()
            .map(|error| unreadable(error, archive_name))
            .collect(),
    };
    let mut named = Vec::new();
    for archive in &archives.archives {
        let live = records
            .workspaces
            .iter()
            .find(|workspace| workspace.name == archive.name);
        let reading = match archive_case(
            backend,
            archive,
            live,
            &archives.archives,
            UnreadPins::CountAsThere,
        ) {
            Ok(reading) => reading,
            // A pin that cannot be read counts as there, so this read
            // never fails.
            Err(_) => continue,
        };
        let Some((problem, repair)) = reading.broken(&archive.name) else {
            continue;
        };
        // A newer workspace of the name is whole, so its row stays as it is.
        let newer = live.is_some_and(|workspace| !same_workspace(archive, workspace));
        let broken = Broken {
            name: archive.name.to_string(),
            repair: Some(repair),
            problem,
        };
        health.archives.push(broken.clone());
        if !newer {
            named.push(broken);
        }
    }
    // A workspace whose archive has not finished is the archive's to
    // repair, even when the archive already deleted every checkout.
    let gone: Vec<Broken> = records
        .workspaces
        .iter()
        .filter(|workspace| {
            !named
                .iter()
                .any(|broken| broken.name == workspace.name.as_str())
        })
        .filter(|workspace| checkouts_gone(workspace))
        .map(|workspace| Broken {
            name: workspace.name.to_string(),
            problem: Problem::CheckoutsGone,
            repair: Some(Repair::Remove(workspace.name.clone())),
        })
        .collect();
    named.extend(gone);
    named.sort_by(|left, right| left.name.cmp(&right.name));
    health.workspaces.extend(named);
    health
}

/// What one archive record and the workspace record of its name are,
/// as [`archive_case`] decides it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArchiveCase {
    /// The archive is still retiring, no workspace has its name, and a
    /// checkout's pin is there, so `mxv archive -w` finishes it.
    Finishable,
    /// The archive is still retiring, no workspace has its name, and the
    /// pin of no checkout names its commit, so finishing it would claim
    /// that a restore brings back what is gone.
    PinsGone,
    /// The archive is still retiring, no workspace has its name, and it is
    /// one of the two records of a rename that a repair stopped, which
    /// [`ArchiveReading::rename`] names.
    Renaming,
    /// The archive is still retiring beside the workspace it archives, so
    /// `mxv archive` resumes it.
    Resumable,
    /// The archive is still retiring, and another workspace has its name.
    BesideNewerWorkspace,
    /// The archive is archived, and the workspace of its name is the one a
    /// restore rebuilt from it, whole, as [`Rebuilt::Whole`] says.
    RestoreUnfinished,
    /// The archive is archived, and a workspace that is another one, or
    /// that a restore rebuilt and is partly gone, has its name.
    ArchivedBesideWorkspace,
    /// The archive is archived, and no workspace has its name.
    Archived,
}

/// What [`archive_case`] read about one archive record.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ArchiveReading<'a> {
    /// What the archive and the workspace of its name are.
    pub(crate) case: ArchiveCase,
    /// The two records of a rename that a repair started and did not
    /// finish, older name first, when the archive is one of them, whatever
    /// the case.
    pub(crate) rename: Option<(&'a Archive, &'a Archive)>,
}

impl ArchiveReading<'_> {
    /// Returns what is wrong with the archive of `name`, and its repair,
    /// or `None` when nothing is. A stopped rename comes first, whatever
    /// state each of its records says, and both of its rows name the
    /// repair of the older name.
    fn broken(&self, name: &WorkspaceName) -> Option<(Problem, Repair)> {
        if let Some((old, _)) = self.rename {
            return Some((Problem::RenameUnfinished, Repair::Repair(old.name.clone())));
        }
        let problem = match self.case {
            ArchiveCase::Finishable | ArchiveCase::Resumable => Problem::ArchiveUnfinished,
            ArchiveCase::PinsGone => Problem::ArchivePinsGone,
            ArchiveCase::Renaming => Problem::RenameUnfinished,
            ArchiveCase::BesideNewerWorkspace => Problem::ArchiveBesideNewerWorkspace,
            ArchiveCase::RestoreUnfinished => Problem::RestoreUnfinished,
            ArchiveCase::ArchivedBesideWorkspace | ArchiveCase::Archived => return None,
        };
        let repair = repair_of(&problem, name);
        Some((problem, repair))
    }
}

/// What [`archive_case`] does when it cannot read a pin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnreadPins {
    /// It returns the error, as `mxv archive -w` does before it finishes
    /// an archive.
    Fail,
    /// It counts the pin as there, so a listing names `mxv archive -w` as
    /// the repair.
    CountAsThere,
}

/// Returns what `archive` and `live`, the workspace record of its name,
/// are. `all` holds every archive record, so a rename that a repair
/// stopped is found. This is the one rule for an unfinished archive:
/// [`classify`], `Workspaces::subject` (`archive/admit.rs`) and
/// [`Workspaces::repair`](crate::Workspaces::repair) each take their
/// answer from it.
///
/// It reads the pins, through `backend`, only for a retiring archive that
/// no workspace has the name of and that is no rename's.
///
/// # Errors
///
/// Returns the failure to read a pin, with [`UnreadPins::Fail`].
pub(crate) fn archive_case<'a, B: Backend>(
    backend: &B,
    archive: &'a Archive,
    live: Option<&Workspace>,
    all: &'a [Archive],
    pins: UnreadPins,
) -> Result<ArchiveReading<'a>, Error> {
    let rename = rename_pair(archive, all);
    let case = match (archive.state, live) {
        (ArchiveState::Retiring, Some(workspace)) if !same_workspace(archive, workspace) => {
            ArchiveCase::BesideNewerWorkspace
        }
        (ArchiveState::Retiring, Some(_)) => ArchiveCase::Resumable,
        // Finishing one record of a rename that stopped would mark an
        // archive whose files are split between two directories.
        (ArchiveState::Retiring, None) if rename.is_some() => ArchiveCase::Renaming,
        (ArchiveState::Retiring, None) => {
            let gone = match pins {
                UnreadPins::Fail => pins_gone(backend, archive)?,
                UnreadPins::CountAsThere => pins_gone(backend, archive).unwrap_or(false),
            };
            if gone {
                ArchiveCase::PinsGone
            } else {
                ArchiveCase::Finishable
            }
        }
        // A workspace of the archive's id whose directories are partly
        // gone keeps its row from `checkouts_gone`, and the archive stays,
        // because it may hold the only copies of the workspace's files.
        (ArchiveState::Archived, Some(workspace))
            if rebuilt(archive, workspace) == Rebuilt::Whole =>
        {
            ArchiveCase::RestoreUnfinished
        }
        (ArchiveState::Archived, Some(_)) => ArchiveCase::ArchivedBesideWorkspace,
        (ArchiveState::Archived, None) => ArchiveCase::Archived,
    };
    Ok(ArchiveReading { case, rename })
}

/// Reports whether `workspace` is the workspace that `archive` archives:
/// the two stable ids are equal, or both records lack one, as an older
/// multiverse wrote them.
pub(crate) fn same_workspace(archive: &Archive, workspace: &Workspace) -> bool {
    workspace.id == archive.workspace_id
}

/// How a workspace record stands to an archive: whether a restore rebuilt
/// the workspace from the archive, and whether all of it is still there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rebuilt {
    /// The workspace is another one: the two stable ids differ, or the
    /// records lack one, as an older multiverse wrote them, so they may be
    /// unrelated.
    Unrelated,
    /// The workspace has the archive's stable id, and its directory and
    /// every checkout's directory are there, so a restore rebuilt it and it
    /// holds every commit again. The archive is then stale.
    Whole,
    /// The workspace has the archive's stable id, and its directory or a
    /// checkout's directory is gone or cannot be read, so the archive may
    /// hold the only copies of the workspace's files.
    Partly,
}

/// Returns how `workspace` stands to `archive`. A restore writes the
/// workspace's record, with the archive's stable id, before it deletes
/// the archive, so only this rule decides whether that archive is stale.
/// `Workspaces::plan_restore`, [`classify`] and the volume guard of
/// `mxv remove --archived` all call it.
#[must_use]
pub fn rebuilt(archive: &Archive, workspace: &Workspace) -> Rebuilt {
    if archive.workspace_id.is_none() || workspace.id != archive.workspace_id {
        return Rebuilt::Unrelated;
    }
    let whole = workspace.path.is_dir()
        && workspace
            .checkouts
            .iter()
            .all(|checkout| checkout.path.is_dir());
    if whole {
        Rebuilt::Whole
    } else {
        Rebuilt::Partly
    }
}

/// Returns the repair of `problem` for the records of `name`.
fn repair_of(problem: &Problem, name: &WorkspaceName) -> Repair {
    match problem {
        Problem::ArchiveUnfinished => Repair::Archive(name.clone()),
        Problem::CheckoutsGone => Repair::Remove(name.clone()),
        Problem::Unreadable { .. }
        | Problem::ArchivePinsGone
        | Problem::ArchiveBesideNewerWorkspace
        | Problem::RenameUnfinished
        | Problem::RestoreUnfinished => Repair::Repair(name.clone()),
    }
}

/// Returns the two records of a rename that a repair started and did not
/// finish, older name first, when `archive` is one of them: another record
/// of `all` holds the same archive id, and one of the two names is a dated
/// name of the other, as `Workspaces::repair` chooses one.
pub(crate) fn rename_pair<'a>(
    archive: &'a Archive,
    all: &'a [Archive],
) -> Option<(&'a Archive, &'a Archive)> {
    all.iter()
        .filter(|other| other.id == archive.id && other.name != archive.name)
        .find_map(|other| {
            if repair::dated_candidates(archive).any(|name| name == other.name) {
                Some((archive, other))
            } else if repair::dated_candidates(other).any(|name| name == archive.name) {
                Some((other, archive))
            } else {
                None
            }
        })
}

/// Reports whether `archive` has checkouts and the pin of no checkout's
/// `HEAD` names its commit any more, as when the mirror that held the pins
/// is gone. An archive without checkouts needs no pin. An archive that
/// lost the pins of only some checkouts still counts as whole, so it is
/// finished. Its restore refuses with `mxv::restore::commit_missing` when
/// no repository holds the commit of such a checkout any more.
///
/// # Errors
///
/// Returns the failure to run git.
pub(crate) fn pins_gone<B: Backend>(backend: &B, archive: &Archive) -> Result<bool, Error> {
    if archive.checkouts.is_empty() {
        return Ok(false);
    }
    for checkout in &archive.checkouts {
        let named = backend.resolve_commit(checkout.pin_holder(), &checkout.pin)?;
        if named.as_deref() == Some(checkout.commit.as_str()) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Returns whether `workspace` has checkouts, every checkout's directory is
/// gone, and the directory that holds the workspace's directory is there.
/// A directory that cannot be read counts as present. When the directory
/// that holds the workspace is gone too, the whole workspace root may lie
/// on a disk that is not mounted, so removing the record is not suggested.
/// The guard assumes that the root lies below the mount point: a workspace
/// directly at an unmounted mount point, which stays as an empty
/// directory, is still named for removal.
pub(crate) fn checkouts_gone(workspace: &Workspace) -> bool {
    let gone = |path: &Path| {
        matches!(
            path.symlink_metadata(),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        )
    };
    !workspace.checkouts.is_empty()
        && workspace.path.parent().is_some_and(|holder| !gone(holder))
        && workspace
            .checkouts
            .iter()
            .all(|checkout| gone(&checkout.path))
}

/// Returns the broken record of `error`, the error of a record that cannot
/// be read, named by `name` from the record's path.
fn unreadable(error: &Error, name: fn(&Path) -> Option<&OsStr>) -> Broken {
    let path = record_path(error);
    Broken {
        name: path
            .and_then(name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        problem: Problem::Unreadable {
            reason: error.to_string(),
        },
        repair: path.map(|path| Repair::MoveAside(path.to_path_buf())),
    }
}

/// Returns the path of the record that `error` names.
fn record_path(error: &Error) -> Option<&Path> {
    match error {
        Error::CorruptRecord { path, .. }
        | Error::NewerRecord { path, .. }
        | Error::MismatchedRecord { path, .. }
        | Error::StateUnreadable { path, .. } => Some(path.as_path()),
        _ => None,
    }
}

/// Returns the name of the archive whose record is at `path`, which is the
/// name of the record's directory.
fn archive_name(path: &Path) -> Option<&OsStr> {
    path.parent().and_then(Path::file_name)
}
