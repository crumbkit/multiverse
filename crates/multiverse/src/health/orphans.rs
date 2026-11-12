//! The archive leftovers that no record names: an archive that a repair
//! moved aside, and a data directory of ignored files whose archive is
//! gone.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::archive::{Archive, ArchiveRecords, Archives};
use crate::error::Error;
use crate::store;
use crate::workspaces::delete_directory;

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
    /// such as the one of a moved-aside archive whose directory a person
    /// deleted by hand. It holds the only copies of those files.
    IgnoredFiles,
}

/// What [`Archives::sweep_orphan`] did with one item.
#[derive(Debug, PartialEq, Eq)]
pub enum Swept {
    /// The item is deleted.
    Deleted,
    /// The item stays, because a fresh listing no longer holds it as it
    /// was, such as a data directory that an archive record names now.
    Kept,
}

impl Archives {
    /// Returns each archive that a repair moved aside and each data
    /// directory that no archive record names, in path order, with their
    /// sizes. It writes nothing and takes no lock.
    ///
    /// # Errors
    ///
    /// Returns the read error of the first archive record that cannot be
    /// read, a moved-aside one included, because such a record may name a
    /// data directory that would otherwise look orphaned, and the errors of
    /// reading the directories.
    pub fn orphans(&self) -> Result<Vec<Orphan>, Error> {
        let leftovers = self.leftovers()?;
        let records = readable(self.list()?)?;
        let mut items = Vec::new();
        let mut aside_ids = BTreeSet::new();
        for (path, record) in &leftovers.aside {
            let data = Some(self.data_of(&record.id)).filter(|data| data.is_dir());
            aside_ids.insert(record.id.as_str());
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
            if live.contains(id.as_str()) || aside_ids.contains(id.as_str()) {
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

    /// Deletes `orphan`, which [`Archives::orphans`] returned, when a fresh
    /// listing still holds an item of the same kind at the same path: a
    /// moved-aside archive and then its data directory, or a data
    /// directory. Otherwise it deletes nothing and returns
    /// [`Swept::Kept`].
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Archives::orphans`] and of the deletions.
    pub fn sweep_orphan(&self, orphan: &Orphan) -> Result<Swept, Error> {
        let current = self
            .orphans()?
            .into_iter()
            .any(|item| item.kind == orphan.kind && item.path == orphan.path);
        if !current {
            return Ok(Swept::Kept);
        }
        delete(&orphan.path)?;
        if let OrphanKind::MovedAside { data: Some(data) } = &orphan.kind {
            delete(data)?;
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

/// Deletes the directory `path` by file descriptor and syncs its parent.
fn delete(path: &Path) -> Result<(), Error> {
    delete_directory(path)?;
    if let Some(parent) = path.parent() {
        store::sync_dir(parent);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{OrphanKind, Swept};
    use crate::archive::Archives;

    #[test]
    fn a_data_directory_that_the_fresh_listing_no_longer_holds_is_kept() {
        let root = tempfile::tempdir().unwrap();
        let archives = Archives::new(root.path().join("state"), root.path().join("data"));
        let data = root.path().join("data/0123456789abcdef0123456789abcdef");
        fs::create_dir_all(&data).unwrap();
        let found = archives.orphans().unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, OrphanKind::IgnoredFiles);
        // Another command deletes the directory between the listing and
        // the sweep, so the fresh listing no longer holds it.
        fs::remove_dir(&data).unwrap();

        assert_eq!(archives.sweep_orphan(&found[0]).unwrap(), Swept::Kept);
    }
}
