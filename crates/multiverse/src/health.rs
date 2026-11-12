//! The health of the records that multiverse keeps.
//!
//! [`classify`] compares each workspace record and each archive record with
//! the state on disk that it should match, and returns a [`Broken`] for
//! each one that does not, with the [`Repair`] that brings it back. It
//! stores nothing and changes nothing, so a listing can call it on every
//! run. [`Workspaces::repair`](crate::Workspaces::repair) performs the
//! repairs that [`Repair::Repair`] names.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use crate::archive::{Archive, ArchiveRecords, ArchiveState};
use crate::backend::Backend;
use crate::error::Error;
use crate::name::WorkspaceName;
use crate::record::Workspace;
use crate::store::Records;

mod repair;

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
    /// name has its stable id, so a restore rebuilt the workspace and
    /// stopped before it deleted the archive.
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
        // A repair that stopped while it moved an older archive to its
        // dated name leaves two records of one archive id, whichever state
        // each says. Both rows name the repair of the older name.
        let (problem, repair) = match rename_pair(archive, &archives.archives) {
            Some((old, _)) => (Problem::RenameUnfinished, Repair::Repair(old.name.clone())),
            None => {
                let Some(problem) = archive_problem(backend, archive, live) else {
                    continue;
                };
                let repair = repair_of(&problem, &archive.name);
                (problem, repair)
            }
        };
        // A newer workspace of the name is whole, so its row stays as it is.
        let newer = live.is_some_and(|workspace| workspace.id != archive.workspace_id);
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

/// Returns what is wrong with `archive`, whose name the workspace `live`
/// holds, or `None` when nothing is.
///
/// `Workspaces::subject` (`archive/admit.rs`) holds the same rule for
/// which retiring archive `mxv archive -w` finishes and which it refuses,
/// so a change to one changes the other.
fn archive_problem<B: Backend>(
    backend: &B,
    archive: &Archive,
    live: Option<&Workspace>,
) -> Option<Problem> {
    match (archive.state, live) {
        (ArchiveState::Retiring, Some(workspace)) if workspace.id != archive.workspace_id => {
            Some(Problem::ArchiveBesideNewerWorkspace)
        }
        (ArchiveState::Retiring, Some(_)) => Some(Problem::ArchiveUnfinished),
        (ArchiveState::Retiring, None) => Some(if pins_gone(backend, archive).unwrap_or(false) {
            Problem::ArchivePinsGone
        } else {
            Problem::ArchiveUnfinished
        }),
        // An archive and a workspace that both lack a stable id, as an
        // older multiverse wrote them, may be unrelated, so only a shared
        // id shows that the restore rebuilt this workspace.
        (ArchiveState::Archived, Some(workspace))
            if archive.workspace_id.is_some() && workspace.id == archive.workspace_id =>
        {
            Some(Problem::RestoreUnfinished)
        }
        (ArchiveState::Archived, _) => None,
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
