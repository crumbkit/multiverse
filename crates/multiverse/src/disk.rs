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
//! newest modification or status-change time (`mtime` or `ctime`) of a file
//! or directory outside the disposable paths and `.git`, and of the `HEAD`,
//! index and reflog of each checkout's git directory, which a commit, a
//! checkout or a staged change writes. A directory's times change when an
//! entry in it is created, renamed or deleted, so a deletion counts as a
//! change. The status-change time cannot be set back, so a file whose
//! modification time a tool restored still counts as changed.
//!
//! A disposable directory is kept whole when it holds a nested repository
//! at any depth, which may hold commits that exist nowhere else, when it
//! holds the mount point of another file system, when it is deeper than
//! [`tree::MOST_DEPTH`], or when mxv cannot read all of it.
//! [`DiskUsage::kept`] lists it with the reason. The checks and the
//! deletion work by file descriptor from the checkout's root, as the
//! `tree` module describes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::backend::Backend;
use crate::disposable::DisposableSource;
use crate::error::Error;
use crate::record::{Checkout, Workspace};
use crate::workspaces::{Entry, Workspaces};

#[cfg(test)]
mod tests;
mod tree;
mod volume;

pub(crate) use tree::keep_reason;

/// Deletes the directory `relative`, such as `app/`, of the directory
/// `root` and everything below it by file descriptor, as [`tree`]
/// describes.
pub(crate) fn remove_tree(root: &Path, relative: &str) -> Result<(), tree::Failure> {
    tree::remove(root, relative, &mut |_| {})
}

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
    /// The newest modification or status-change time of an entry outside
    /// the disposable paths or of a checkout's git `HEAD`, index or reflog,
    /// or `None` when the workspace holds no such entry.
    pub modified: Option<SystemTime>,
    /// Each disposable path, in the order that `git status` reports them,
    /// checkout by checkout.
    pub disposable: Vec<DisposablePath>,
    /// Each path that the disposable set matches and that mxv keeps, with
    /// the reason, such as a directory that holds a nested repository.
    pub kept: Vec<KeptPath>,
    /// Why a checkout's disposable set holds fewer globs than its
    /// `mxv.toml` declares, such as an invalid manifest.
    pub warnings: Vec<String>,
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
    /// The bytes that the path takes on disk. In the result of
    /// [`Workspaces::clean`], the bytes that deleting it freed.
    pub bytes: u64,
    /// What made the path disposable: a built-in name, the config file, or
    /// only the repository's `mxv.toml`.
    pub source: DisposableSource,
}

/// A path that the disposable set matches and that mxv keeps.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct KeptPath {
    /// The checkout's name, such as `app`.
    pub checkout: String,
    /// The path relative to the checkout, as `git status` prints it.
    pub path: String,
    /// Why mxv keeps it.
    pub reason: String,
}

impl KeptPath {
    /// Returns the path `path` of the checkout `checkout`, which mxv keeps
    /// for `reason`.
    #[must_use]
    pub fn new(checkout: String, path: String, reason: String) -> Self {
        Self {
            checkout,
            path,
            reason,
        }
    }
}

/// A planned path that [`Workspaces::clean`] could not delete in full.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct FailedPath {
    /// The checkout's name, such as `app`.
    pub checkout: String,
    /// The planned path relative to the checkout, such as `target/`.
    pub path: String,
    /// The first file or directory below it that could not be deleted.
    pub failed: PathBuf,
    /// Why it could not be deleted.
    pub error: String,
    /// The bytes that the deletion freed before and after the failure.
    pub bytes: u64,
}

/// What [`Workspaces::clean`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Cleaned {
    /// Why the workspace was busy when the check ran, in which case
    /// nothing was deleted.
    pub busy: Option<String>,
    /// Each planned path that was deleted, with the bytes that deleting it
    /// freed.
    pub deleted: Vec<DisposablePath>,
    /// Each planned path that was kept, with the reason.
    pub kept: Vec<KeptPath>,
    /// Each planned path whose deletion failed partway.
    pub failed: Vec<FailedPath>,
    /// The warnings of the fresh measurement that the run made first.
    pub warnings: Vec<String>,
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
    /// The newest modification or status-change time of an entry outside
    /// the disposable paths.
    modified: Option<SystemTime>,
}

impl Walk {
    /// Notes the modification and status-change times of `metadata`.
    fn touched(&mut self, metadata: &Metadata) {
        if let Some(time) = changed(metadata) {
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
            None if !git => self.touched(metadata),
            None => {}
        }
        is_directory
    }
}

/// Returns the later of the modification and status-change times of
/// `metadata`.
#[cfg(unix)]
fn changed(metadata: &Metadata) -> Option<SystemTime> {
    use std::os::unix::fs::MetadataExt as _;
    use std::time::Duration;
    let status = u64::try_from(metadata.ctime()).ok().and_then(|seconds| {
        let nanos = u32::try_from(metadata.ctime_nsec()).unwrap_or_default();
        SystemTime::UNIX_EPOCH.checked_add(Duration::new(seconds, nanos))
    });
    match (metadata.modified().ok(), status) {
        (Some(modified), Some(status)) => Some(modified.max(status)),
        (modified, status) => modified.or(status),
    }
}

/// Returns the modification time of `metadata`, where no status-change
/// time is known.
#[cfg(not(unix))]
fn changed(metadata: &Metadata) -> Option<SystemTime> {
    metadata.modified().ok()
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

/// Returns the bytes that deleting the tree at `root` frees: the blocks of
/// each directory, and of each file whose every hard link lies inside the
/// tree. A file with a hard link elsewhere, such as in a package store,
/// frees nothing.
fn freeable(root: &Path) -> u64 {
    let mut files: HashMap<(u64, u64), (u64, u64, u64)> = HashMap::new();
    let mut total: u64 = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            total = total.saturating_add(on_disk(&metadata));
            if let Ok(entries) = fs::read_dir(&path) {
                stack.extend(entries.flatten().map(|entry| entry.path()));
            }
            continue;
        }
        if links(&metadata) <= 1 {
            total = total.saturating_add(on_disk(&metadata));
            continue;
        }
        let seen =
            files
                .entry(inode(&metadata))
                .or_insert((on_disk(&metadata), links(&metadata), 0));
        seen.2 = seen.2.saturating_add(1);
    }
    files
        .values()
        .filter(|(_, links, seen)| seen >= links)
        .fold(total, |sum, (bytes, _, _)| sum.saturating_add(*bytes))
}

impl<B: Backend> Workspaces<B> {
    /// Returns the disposable paths of each checkout of `workspace` that
    /// exists, as `git status` reports its ignored paths and the
    /// checkout's disposable set matches them, without their sizes, the
    /// matched paths that hold a nested repository, and the warnings of the
    /// disposable sets.
    #[expect(
        clippy::type_complexity,
        reason = "a private helper returns its three lists to one caller"
    )]
    fn disposable_paths(
        &self,
        workspace: &Workspace,
    ) -> Result<(Vec<DisposablePath>, Vec<KeptPath>, Vec<String>), Error> {
        let mut found = Vec::new();
        let mut kept = Vec::new();
        let mut warnings = Vec::new();
        for checkout in &workspace.checkouts {
            if Entry::at(&checkout.path)? != Entry::Present || !checkout.path.is_dir() {
                continue;
            }
            let (set, warning) = self.disposable_set(workspace, checkout);
            warnings.extend(warning);
            let changes = self.backend.changes(&checkout.path)?;
            for path in changes.ignored {
                let Some(source) = set.source(&path) else {
                    continue;
                };
                if let Some(reason) = keep_reason(&checkout.path, &path) {
                    kept.push(KeptPath {
                        checkout: checkout.name.clone(),
                        reason,
                        path,
                    });
                    continue;
                }
                found.push(DisposablePath {
                    checkout: checkout.name.clone(),
                    path,
                    bytes: 0,
                    source,
                });
            }
        }
        Ok((found, kept, warnings))
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
        let (mut disposable, kept, warnings) = self.disposable_paths(workspace)?;
        let checkout_of = |found: &DisposablePath| -> Option<&Checkout> {
            workspace
                .checkouts
                .iter()
                .find(|checkout| checkout.name == found.checkout)
        };
        let mut marks: BTreeMap<PathBuf, usize> = disposable
            .iter()
            .enumerate()
            .filter_map(|(index, found)| {
                checkout_of(found).map(|checkout| (absolute(&checkout.path, &found.path), index))
            })
            .collect();
        // A kept path is neither disposable nor a change of the checkout,
        // so it gets a mark past the plan's that no total reads.
        for (offset, path) in kept.iter().enumerate() {
            if let Some(checkout) = workspace
                .checkouts
                .iter()
                .find(|checkout| checkout.name == path.checkout)
            {
                marks.insert(
                    absolute(&checkout.path, &path.path),
                    disposable.len().saturating_add(offset),
                );
            }
        }
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
            disposable_bytes: disposable
                .iter()
                .fold(0, |sum, found| sum.saturating_add(found.bytes)),
            modified: totals.modified,
            disposable,
            kept,
            warnings,
        })
    }

    /// Deletes the paths of `planned`, which [`Workspaces::disk_usage`]
    /// returned and the caller confirmed, from the checkouts of
    /// `workspace`, and returns what it deleted, kept and failed to delete.
    /// The caller holds the workspace's lock, so no `mxv up` starts it
    /// meanwhile.
    ///
    /// It measures the workspace again first and passes the measurement to
    /// `check`, which returns why the workspace is busy, such as a process
    /// that runs or a file that changed since the plan. A busy workspace
    /// keeps every path. Otherwise it deletes a planned path only while git
    /// still reports it as an ignored path that the disposable set matches,
    /// it holds no `.git` entry, it is not a symbolic link, its parent
    /// directory resolves to a directory inside the checkout, and the
    /// repository tracks no file at or below it. A planned path that fails
    /// a check is kept with the reason. It deletes nothing else: no tracked
    /// file, no git data, and no ignored path outside the disposable set.
    ///
    /// A path whose deletion fails partway is reported with the first entry
    /// that could not be deleted, and the run goes on with the next path.
    /// The bytes freed are measured before and after each deletion.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Workspaces::disk_usage`], the error of
    /// `check`, and the failure of the git command that lists tracked
    /// files.
    pub fn clean<E: From<Error>>(
        &self,
        workspace: &Workspace,
        planned: &[DisposablePath],
        check: impl FnOnce(&DiskUsage) -> Result<Option<String>, E>,
    ) -> Result<Cleaned, E> {
        let fresh = self.disk_usage(workspace)?;
        let mut cleaned = Cleaned {
            warnings: fresh.warnings.clone(),
            ..Cleaned::default()
        };
        if let Some(reason) = check(&fresh)? {
            cleaned.busy = Some(reason);
            return Ok(cleaned);
        }
        let planned_here = |checkout: &str, path: &str| {
            planned
                .iter()
                .any(|plan| plan.checkout == checkout && plan.path == path)
        };
        for found in fresh.kept {
            if planned_here(&found.checkout, &found.path) {
                cleaned.kept.push(found);
            }
        }
        for plan in planned {
            let still = fresh
                .disposable
                .iter()
                .any(|found| found.checkout == plan.checkout && found.path == plan.path);
            if !still
                && !cleaned
                    .kept
                    .iter()
                    .any(|kept| kept.path == plan.path && kept.checkout == plan.checkout)
            {
                cleaned.kept.push(KeptPath {
                    checkout: plan.checkout.clone(),
                    path: plan.path.clone(),
                    reason: "git no longer reports it as an ignored disposable path".to_owned(),
                });
            }
        }
        for found in fresh.disposable {
            if !planned_here(&found.checkout, &found.path) {
                continue;
            }
            let Some(checkout) = workspace
                .checkouts
                .iter()
                .find(|checkout| checkout.name == found.checkout)
            else {
                continue;
            };
            if let Some(reason) = self.undeletable(checkout, &found.path)? {
                cleaned.kept.push(KeptPath {
                    checkout: found.checkout,
                    path: found.path,
                    reason,
                });
                continue;
            }
            let path = absolute(&checkout.path, &found.path);
            let before = freeable(&path);
            let removed = tree::remove(&checkout.path, &found.path, &mut |_| {});
            let after = if path.exists() { freeable(&path) } else { 0 };
            let bytes = before.saturating_sub(after);
            match removed {
                Ok(()) => cleaned.deleted.push(DisposablePath { bytes, ..found }),
                Err((failed, error)) => cleaned.failed.push(FailedPath {
                    checkout: found.checkout,
                    path: found.path,
                    failed,
                    error: error.to_string(),
                    bytes,
                }),
            }
        }
        Ok(cleaned)
    }

    /// Returns why the disposable `path` of `checkout` fails the checks of
    /// [`Workspaces::clean`] that come after git's report, or `None` when
    /// it passes them.
    fn undeletable(&self, checkout: &Checkout, path: &str) -> Result<Option<String>, Error> {
        let target = absolute(&checkout.path, path);
        let Ok(metadata) = fs::symlink_metadata(&target) else {
            return Ok(Some("it is gone".to_owned()));
        };
        if metadata.is_symlink() {
            return Ok(Some("it is a symbolic link".to_owned()));
        }
        if metadata.is_dir() != path.ends_with('/') {
            return Ok(Some("its kind changed since the plan".to_owned()));
        }
        let (Ok(root), Some(Ok(parent))) = (
            checkout.path.canonicalize(),
            target.parent().map(Path::canonicalize),
        ) else {
            return Ok(Some("mxv cannot resolve its parent directory".to_owned()));
        };
        if !parent.starts_with(&root) {
            return Ok(Some(
                "its parent directory lies outside the checkout".to_owned(),
            ));
        }
        if let Some(reason) = keep_reason(&checkout.path, path) {
            return Ok(Some(reason));
        }
        if self.backend.tracks(&checkout.path, path)? {
            return Ok(Some("the repository tracks a file in it".to_owned()));
        }
        Ok(None)
    }
}
