//! The health of the records that multiverse keeps.
//!
//! [`classify`] compares each workspace record and each archive record with
//! the state on disk that it should match, and returns a [`Broken`] for
//! each one that does not, with the [`Repair`] that brings it back. It
//! stores nothing and changes nothing, so a listing can call it on every
//! run.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use crate::archive::{Archive, ArchiveRecords, ArchiveState};
use crate::error::Error;
use crate::name::WorkspaceName;
use crate::record::Workspace;
use crate::store::Records;

#[cfg(test)]
mod tests;

/// The broken records that [`classify`] found, by the listing that shows
/// them.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct Health {
    /// The broken records that a listing of the workspaces shows: each
    /// workspace record that cannot be read or does not match its
    /// directory, and each archive that has not finished. The records that
    /// cannot be read come first, and then the others in name order.
    pub workspaces: Vec<Broken>,
    /// The broken records that a listing of the archives shows: each
    /// archive record that cannot be read or that has not finished, in the
    /// same order.
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
    /// What brings the record back, or `None` when no command can yet.
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
    /// workspace has its name or the workspace of that name is the one it
    /// archives.
    ArchiveUnfinished,
    /// The archive is still [`ArchiveState::Retiring`], and a workspace
    /// with another stable id has its name, so archiving the name again
    /// refuses.
    ArchiveBesideNewerWorkspace,
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
    /// Moving the record file at this path aside by hand. multiverse never
    /// deletes a record that it cannot read.
    MoveAside(PathBuf),
}

/// Returns each record of `records` and `archives` that does not match the
/// state it should be in, with its repair.
///
/// It reads only whether each checkout's directory exists.
#[must_use]
pub fn classify(records: &Records, archives: &ArchiveRecords) -> Health {
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
        if archive.state != ArchiveState::Retiring {
            continue;
        }
        let live = records
            .workspaces
            .iter()
            .find(|workspace| workspace.name == archive.name);
        match live {
            Some(workspace) if workspace.id != archive.workspace_id => {
                health.archives.push(Broken {
                    name: archive.name.to_string(),
                    problem: Problem::ArchiveBesideNewerWorkspace,
                    repair: None,
                });
            }
            _ => {
                let broken = unfinished(archive);
                health.archives.push(broken.clone());
                named.push(broken);
            }
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

/// Returns the broken record of `archive`, which has not finished.
fn unfinished(archive: &Archive) -> Broken {
    Broken {
        name: archive.name.to_string(),
        problem: Problem::ArchiveUnfinished,
        repair: Some(Repair::Archive(archive.name.clone())),
    }
}

/// Returns whether `workspace` has checkouts, every checkout's directory is
/// gone, and the directory that holds the workspace's directory is there.
/// A directory that cannot be read counts as present. When the directory
/// that holds the workspace is gone too, the whole workspace root may lie
/// on a disk that is not mounted, so removing the record is not suggested.
fn checkouts_gone(workspace: &Workspace) -> bool {
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
