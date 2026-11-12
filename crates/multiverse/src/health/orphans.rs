//! The archive leftovers that no record names, which
//! [`Archives::orphans`] finds and [`Archives::sweep_orphan`] deletes: an
//! archive that a repair moved aside, and a data directory of ignored
//! files whose archive is gone.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::archive::{Archive, ArchiveRecords, Archives};
use crate::error::Error;

/// One item that no record names.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Orphan {
    /// What the item is.
    pub kind: OrphanKind,
    /// The directory.
    pub path: PathBuf,
    /// The bytes that deleting it frees, so that a person decides about it
    /// by its size.
    pub bytes: u64,
}

/// What an [`Orphan`] is. The enum is exhaustive, so a caller that prints
/// each kind must handle a new one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OrphanKind {
    /// An archive that `mxv repair NAME` moved aside to `.<name>.<id>`,
    /// because its pins were gone. It holds the only copies of the
    /// workspace's root files.
    MovedAside {
        /// The data directory of its ignored files, or `None` when it has
        /// none.
        data: Option<PathBuf>,
    },
    /// A data directory of ignored files whose archive id no record names,
    /// such as one that the deletion of an archive left when it stopped
    /// between its two directories. It holds the only copies of those
    /// files.
    IgnoredFiles,
}

/// What [`Archives::sweep_orphan`] did with one item.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Swept {
    /// The item is deleted.
    Deleted,
    /// The item stays, for the reasons that each sentence gives.
    Kept(Vec<String>),
}

impl Archives {
    /// Returns each archive that a repair moved aside and each data
    /// directory that no archive record names, in path order, with their
    /// sizes. It reads and writes nothing else, and takes no lock.
    ///
    /// # Errors
    ///
    /// Returns the read error of the first archive record that cannot be
    /// read, because such a record may name a data directory that would
    /// otherwise look orphaned, and the errors of reading the directories.
    pub fn orphans(&self) -> Result<Vec<Orphan>, Error> {
        // The data directories are read before the records, as
        // `leftovers` says, so a running archive's data directory is never
        // listed.
        let leftovers = self.leftovers()?;
        let records = readable(self.list()?)?;
        let mut items = Vec::new();
        let mut aside_ids = BTreeSet::new();
        for (path, record) in &leftovers.aside {
            let id = record
                .as_ref()
                .map(|archive| archive.id.clone())
                .or_else(|| aside_id(path));
            let data = id
                .as_deref()
                .map(|id| self.data_of(id))
                .filter(|data| data.is_dir());
            aside_ids.extend(id);
            let bytes = crate::disk::freeable(path)
                .saturating_add(data.as_deref().map_or(0, crate::disk::freeable));
            items.push(Orphan {
                kind: OrphanKind::MovedAside { data },
                path: path.clone(),
                bytes,
            });
        }
        let live: BTreeSet<&str> = records.iter().map(|archive| archive.id.as_str()).collect();
        for (id, path) in &leftovers.data {
            if live.contains(id.as_str()) || aside_ids.contains(id) {
                continue;
            }
            items.push(Orphan {
                kind: OrphanKind::IgnoredFiles,
                path: path.clone(),
                bytes: crate::disk::freeable(path),
            });
        }
        Ok(items)
    }

    /// Deletes `orphan`, which [`Archives::orphans`] returned, after it
    /// reads the archive records again: a moved-aside archive with its
    /// data directory, or a data directory that still no record names.
    ///
    /// # Errors
    ///
    /// Returns the read error of an archive record that cannot be read,
    /// and the errors of the deletions.
    pub fn sweep_orphan(&self, orphan: &Orphan) -> Result<Swept, Error> {
        match &orphan.kind {
            OrphanKind::MovedAside { data } => {
                Self::delete_leftover(&orphan.path)?;
                if let Some(data) = data {
                    Self::delete_leftover(data)?;
                }
            }
            OrphanKind::IgnoredFiles => {
                let id = orphan.path.file_name().and_then(|name| name.to_str());
                let named = readable(self.list()?)?
                    .iter()
                    .any(|archive| Some(archive.id.as_str()) == id);
                if named {
                    return Ok(Swept::Kept(vec![
                        "an archive record names it now".to_owned(),
                    ]));
                }
                Self::delete_leftover(&orphan.path)?;
            }
        }
        Ok(Swept::Deleted)
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

/// Returns the archive id of a directory `.<name>.<id>` that a repair
/// moved aside, read from its name.
fn aside_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let (_, id) = name.rsplit_once('.')?;
    Some(id.to_owned())
}
