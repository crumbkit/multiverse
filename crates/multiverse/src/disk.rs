//! The disk use of a workspace, when its checkouts last changed, and the
//! deletion of their disposable paths.
//!
//! [`Workspaces::disk_usage`] walks a workspace's directory once. It counts
//! the bytes that each file takes on disk, which are its allocated blocks
//! and may be fewer or more than its length. It counts a file with several
//! hard links once, and it counts a symbolic link as the link itself and
//! never follows it. An entry that cannot be read, such as a directory
//! without read permission, counts as no bytes, so the sizes can fall
//! short and never fail. APFS clones, such as the `node_modules/` that
//! `mxv new` clones, share their blocks until a file changes, and each
//! clone counts its blocks in full, so the sizes of cloned directories
//! overstate what they free.
//!
//! The same walk finds when the workspace's checkouts last changed: the
//! newest modification time of a file outside the disposable paths, and of
//! the `HEAD`, index and reflog of each checkout's git directory, which a
//! commit, a checkout or a staged change writes. Directories do not count,
//! because deleting a disposable directory changes its parent's time.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::backend::Backend;
use crate::error::Error;
use crate::record::{Checkout, Workspace};
use crate::workspaces::{Entry, Workspaces};

#[cfg(test)]
mod tests;

/// The files of a checkout's git directory that a commit, a checkout or a
/// staged change writes.
const GIT_FILES: &[&str] = &["HEAD", "index", "logs/HEAD"];

/// A workspace's disk use and when its checkouts last changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DiskUsage {
    /// The bytes that the workspace's directory takes on disk.
    pub bytes: u64,
    /// The bytes that the disposable paths take on disk.
    pub disposable_bytes: u64,
    /// The newest modification time of a file outside the disposable paths
    /// or of a checkout's git `HEAD`, index or reflog, or `None` when the
    /// workspace holds no such file.
    pub modified: Option<SystemTime>,
    /// Each disposable path, in the order that `git status` reports them,
    /// checkout by checkout.
    pub disposable: Vec<DisposablePath>,
}

/// A disposable path of a checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DisposablePath {
    /// The checkout's name, such as `app`.
    pub checkout: String,
    /// The path relative to the checkout, as `git status` prints it, such
    /// as `target/` or `web/node_modules/`.
    pub path: String,
    /// The bytes that the path takes on disk.
    pub bytes: u64,
}

/// What one walk of a workspace's directory adds up.
#[derive(Default)]
struct Walk {
    /// The device and inode of each file with several hard links that the
    /// walk counted.
    linked: HashSet<(u64, u64)>,
    /// The bytes of every entry.
    bytes: u64,
    /// The bytes of each disposable path, by its index in the plan.
    marked: BTreeMap<usize, u64>,
    /// The newest modification time of a file outside the disposable
    /// paths.
    modified: Option<SystemTime>,
}

impl Walk {
    /// Notes the modification time of `metadata`.
    fn touched(&mut self, metadata: &Metadata) {
        if let Ok(time) = metadata.modified() {
            self.modified = Some(self.modified.map_or(time, |newest| newest.max(time)));
        }
    }

    /// Adds the entry `metadata`, inside the disposable path `mark` or
    /// outside every one, and returns whether it is a directory to descend
    /// into.
    fn add(&mut self, metadata: &Metadata, mark: Option<usize>, git: bool) -> bool {
        let is_directory = metadata.is_dir();
        if !is_directory && links(metadata) > 1 && !self.linked.insert(inode(metadata)) {
            return false;
        }
        let bytes = on_disk(metadata);
        self.bytes = self.bytes.saturating_add(bytes);
        match mark {
            Some(index) => {
                let total = self.marked.entry(index).or_default();
                *total = total.saturating_add(bytes);
            }
            None if !is_directory && !git => self.touched(metadata),
            None => {}
        }
        is_directory
    }
}

/// Returns the bytes that the entry of `metadata` takes on disk: its
/// allocated 512-byte blocks.
#[cfg(unix)]
fn on_disk(metadata: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    metadata.blocks().saturating_mul(512)
}

/// Returns the length of the entry of `metadata`, where no block count is
/// known.
#[cfg(not(unix))]
fn on_disk(metadata: &Metadata) -> u64 {
    metadata.len()
}

/// Returns the number of hard links of the entry of `metadata`.
#[cfg(unix)]
fn links(metadata: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    metadata.nlink()
}

/// Returns 1, where hard links are not counted.
#[cfg(not(unix))]
fn links(_metadata: &Metadata) -> u64 {
    1
}

/// Returns the device and inode of the entry of `metadata`.
#[cfg(unix)]
fn inode(metadata: &Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt as _;
    (metadata.dev(), metadata.ino())
}

/// Returns nothing that tells two entries apart, where hard links are not
/// counted.
#[cfg(not(unix))]
fn inode(_metadata: &Metadata) -> (u64, u64) {
    (0, 0)
}

/// Walks `root` without following symbolic links. `marks` maps the
/// absolute path of each disposable path to its index, and `git` holds the
/// `.git` entry of each checkout, whose times, and the times of the files
/// below it, do not count.
fn walk(root: &Path, marks: &BTreeMap<PathBuf, usize>, git: &HashSet<PathBuf>, totals: &mut Walk) {
    let mut stack = vec![(root.to_path_buf(), None, false)];
    while let Some((path, mark, in_git)) = stack.pop() {
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        let mark = mark.or_else(|| marks.get(&path).copied());
        let in_git = in_git || git.contains(&path);
        if !totals.add(&metadata, mark, in_git) {
            continue;
        }
        let Ok(entries) = fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            stack.push((entry.path(), mark, in_git));
        }
    }
}

/// Returns the git directory of the checkout at `checkout`: the directory
/// that its `.git` file names, or its `.git` directory.
fn git_dir(checkout: &Path) -> Option<PathBuf> {
    let dot_git = checkout.join(".git");
    let metadata = fs::symlink_metadata(&dot_git).ok()?;
    if metadata.is_dir() {
        return Some(dot_git);
    }
    let text = fs::read_to_string(&dot_git).ok()?;
    let named = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    Some(checkout.join(named))
}

/// Returns the path of `path` in the checkout at `checkout`, without the
/// trailing `/` of a directory.
fn absolute(checkout: &Path, path: &str) -> PathBuf {
    checkout.join(path.trim_end_matches('/'))
}

impl<B: Backend> Workspaces<B> {
    /// Returns the disposable paths of each checkout of `workspace` that
    /// exists, as `git status` reports its ignored paths and the
    /// checkout's disposable set matches them, without their sizes.
    fn disposable_paths(&self, workspace: &Workspace) -> Result<Vec<DisposablePath>, Error> {
        let mut found = Vec::new();
        for checkout in &workspace.checkouts {
            if Entry::at(&checkout.path)? != Entry::Present || !checkout.path.is_dir() {
                continue;
            }
            let set = self.disposable_set(checkout);
            let changes = self.backend.changes(&checkout.path)?;
            found.extend(
                changes
                    .ignored
                    .into_iter()
                    .filter(|path| set.matches(path))
                    .map(|path| DisposablePath {
                        checkout: checkout.name.clone(),
                        path,
                        bytes: 0,
                    }),
            );
        }
        Ok(found)
    }

    /// Returns the disk use of `workspace` and when its checkouts last
    /// changed, as the module docs describe. A workspace whose directory is
    /// gone uses no bytes.
    ///
    /// # Errors
    ///
    /// Returns the failure of `git status` in a checkout, and
    /// [`Error::Inaccessible`] when mxv cannot tell whether a checkout's
    /// directory exists.
    pub fn disk_usage(&self, workspace: &Workspace) -> Result<DiskUsage, Error> {
        let mut disposable = self.disposable_paths(workspace)?;
        let checkout_of = |found: &DisposablePath| -> Option<&Checkout> {
            workspace
                .checkouts
                .iter()
                .find(|checkout| checkout.name == found.checkout)
        };
        let marks: BTreeMap<PathBuf, usize> = disposable
            .iter()
            .enumerate()
            .filter_map(|(index, found)| {
                checkout_of(found).map(|checkout| (absolute(&checkout.path, &found.path), index))
            })
            .collect();
        let git: HashSet<PathBuf> = workspace
            .checkouts
            .iter()
            .map(|checkout| checkout.path.join(".git"))
            .collect();
        let mut totals = Walk::default();
        if Entry::at(&workspace.path)? == Entry::Present {
            walk(&workspace.path, &marks, &git, &mut totals);
        }
        for checkout in &workspace.checkouts {
            let Some(dir) = git_dir(&checkout.path) else {
                continue;
            };
            for file in GIT_FILES {
                if let Ok(metadata) = fs::symlink_metadata(dir.join(file)) {
                    totals.touched(&metadata);
                }
            }
        }
        for (index, found) in disposable.iter_mut().enumerate() {
            found.bytes = totals.marked.get(&index).copied().unwrap_or_default();
        }
        Ok(DiskUsage {
            bytes: totals.bytes,
            disposable_bytes: totals
                .marked
                .values()
                .fold(0, |sum, bytes| sum.saturating_add(*bytes)),
            modified: totals.modified,
            disposable,
        })
    }

    /// Deletes the paths of `planned`, which [`Workspaces::disk_usage`]
    /// returned and the caller confirmed, from the checkouts of
    /// `workspace`, and returns each path it deleted with the bytes that it
    /// took.
    ///
    /// It asks git for the disposable paths again first, and deletes a
    /// planned path only while git still reports it as an ignored path
    /// that the disposable set matches, it is not a symbolic link, its
    /// parent directory resolves to a directory inside the checkout, and
    /// the repository tracks no file at or below it. A planned path that
    /// fails a check stays, and is left out of the result. It deletes
    /// nothing else: no tracked file, no git data, and no ignored path
    /// outside the disposable set.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Workspaces::disk_usage`], the failure of the
    /// git command that lists tracked files, and [`Error::Unwritable`] when
    /// a path cannot be deleted. The paths before the failure stay deleted.
    pub fn clean(
        &self,
        workspace: &Workspace,
        planned: &[DisposablePath],
    ) -> Result<Vec<DisposablePath>, Error> {
        let fresh = self.disk_usage(workspace)?;
        let mut cleaned = Vec::new();
        for found in fresh.disposable {
            let confirmed = planned
                .iter()
                .any(|plan| plan.checkout == found.checkout && plan.path == found.path);
            let Some(checkout) = workspace
                .checkouts
                .iter()
                .find(|checkout| checkout.name == found.checkout)
            else {
                continue;
            };
            if !confirmed || !self.deletable(checkout, &found.path)? {
                continue;
            }
            let path = absolute(&checkout.path, &found.path);
            let deleted = if found.path.ends_with('/') {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            match deleted {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(source) => return Err(Error::Unwritable { path, source }),
            }
            cleaned.push(found);
        }
        Ok(cleaned)
    }

    /// Reports whether the disposable `path` of `checkout` passes the
    /// checks of [`Workspaces::clean`] that come after git's report.
    fn deletable(&self, checkout: &Checkout, path: &str) -> Result<bool, Error> {
        let target = absolute(&checkout.path, path);
        let Ok(metadata) = fs::symlink_metadata(&target) else {
            return Ok(false);
        };
        if metadata.is_symlink() || metadata.is_dir() != path.ends_with('/') {
            return Ok(false);
        }
        let (Ok(root), Some(Ok(parent))) = (
            checkout.path.canonicalize(),
            target.parent().map(Path::canonicalize),
        ) else {
            return Ok(false);
        };
        if !parent.starts_with(&root) {
            return Ok(false);
        }
        Ok(!self.backend.tracks(&checkout.path, path)?)
    }
}
