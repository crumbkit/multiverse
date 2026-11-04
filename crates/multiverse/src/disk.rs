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
//! A disposable directory that holds a `.git` entry at any depth, such as
//! a nested repository, is kept, because it may hold commits that exist
//! nowhere else. [`DiskUsage::kept`] lists it with the reason.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, Metadata};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, openat, statat, unlinkat};
use rustix::io::Errno;

use crate::backend::Backend;
use crate::disposable::DisposableSource;
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

/// Returns the first nested repository at any depth below the ignored
/// path `path` of the checkout at `checkout`, relative to the checkout, or
/// `None` when it holds none or is not a directory. A nested repository is
/// an entry named `.git` in any case, or a directory that looks like a bare
/// repository because it holds `HEAD`, `objects/` and `refs/`. It never
/// follows a symbolic link.
pub(crate) fn nested_git(checkout: &Path, path: &str) -> Option<String> {
    if !path.ends_with('/') {
        return None;
    }
    let relative = |found: &Path| {
        found
            .strip_prefix(checkout)
            .unwrap_or(found)
            .display()
            .to_string()
    };
    let mut stack = vec![absolute(checkout, path)];
    while let Some(dir) = stack.pop() {
        if is_bare(&dir) {
            return Some(format!("{}/", relative(&dir)));
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let found = entry.path();
            if entry.file_name().eq_ignore_ascii_case(".git") {
                return Some(relative(&found));
            }
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                stack.push(found);
            }
        }
    }
    None
}

/// Reports whether the directory `dir` looks like a bare repository: it
/// holds a `HEAD` file and the directories `objects` and `refs`.
fn is_bare(dir: &Path) -> bool {
    let kind = |name: &str| fs::symlink_metadata(dir.join(name)).ok();
    kind("HEAD").is_some_and(|metadata| metadata.is_file())
        && kind("objects").is_some_and(|metadata| metadata.is_dir())
        && kind("refs").is_some_and(|metadata| metadata.is_dir())
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

/// Deletes the tree at `path` without following symbolic links, as
/// [`remove_tree_with`] describes.
fn remove_tree(path: &Path) -> Result<(), (PathBuf, io::Error)> {
    remove_tree_with(path, &mut |_| {})
}

/// Deletes the tree at `path` and calls `removed` with each entry it
/// deleted. A failure below `path` does not stop the deletion of its other
/// entries, and the result names the first entry that could not be
/// deleted.
///
/// The deletion works by file descriptor, as `std::fs::remove_dir_all`
/// does. It opens each directory relative to its parent with
/// `O_NOFOLLOW` and deletes its entries relative to that descriptor, so a
/// directory that another process replaces with a symbolic link during
/// the deletion is never followed. The link itself is deleted, or the
/// deletion of its name fails, and nothing outside the tree is touched.
fn remove_tree_with(
    path: &Path,
    removed: &mut dyn FnMut(&Path),
) -> Result<(), (PathBuf, io::Error)> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err((path.to_path_buf(), io::ErrorKind::InvalidInput.into()));
    };
    let parent_fd = openat(
        CWD,
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| (parent.to_path_buf(), io::Error::from(error)))?;
    remove_entry(&parent_fd, name, path, removed)
}

/// Deletes the entry `name` of the open directory `dir`, whose path is
/// `path`, and everything below it.
fn remove_entry(
    dir: &OwnedFd,
    name: &OsStr,
    path: &Path,
    removed: &mut dyn FnMut(&Path),
) -> Result<(), (PathBuf, io::Error)> {
    let fail = |error: Errno| (path.to_path_buf(), io::Error::from(error));
    let stat = match statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(Errno::NOENT) => return Ok(()),
        Err(error) => return Err(fail(error)),
    };
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
        return unlink(dir, name, AtFlags::empty(), path, removed);
    }
    let child = match openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(child) => child,
        // The entry stopped being a directory after the stat, such as a
        // directory that was swapped for a symbolic link, so only the
        // entry itself is deleted.
        Err(Errno::LOOP | Errno::NOTDIR) => {
            return unlink(dir, name, AtFlags::empty(), path, removed);
        }
        Err(Errno::NOENT) => return Ok(()),
        Err(error) => return Err(fail(error)),
    };
    let mut names = Vec::new();
    for entry in Dir::read_from(&child).map_err(fail)? {
        let entry = entry.map_err(fail)?;
        let bytes = entry.file_name().to_bytes();
        if bytes != b"." && bytes != b".." {
            names.push(OsStr::from_bytes(bytes).to_os_string());
        }
    }
    let mut first = None;
    for entry in names {
        if let Err(failure) = remove_entry(&child, &entry, &path.join(&entry), removed) {
            first.get_or_insert(failure);
        }
    }
    if let Some(failure) = first {
        return Err(failure);
    }
    unlink(dir, name, AtFlags::REMOVEDIR, path, removed)
}

/// Deletes the entry `name` of the open directory `dir`, whose path is
/// `path`, with `unlinkat` and `flags`, and calls `removed` with it.
fn unlink(
    dir: &OwnedFd,
    name: &OsStr,
    flags: AtFlags,
    path: &Path,
    removed: &mut dyn FnMut(&Path),
) -> Result<(), (PathBuf, io::Error)> {
    match unlinkat(dir, name, flags) {
        Ok(()) => {
            removed(path);
            Ok(())
        }
        Err(Errno::NOENT) => Ok(()),
        Err(error) => Err((path.to_path_buf(), io::Error::from(error))),
    }
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
            let (set, warning) = self.disposable_set(checkout);
            warnings.extend(warning);
            let changes = self.backend.changes(&checkout.path)?;
            for path in changes.ignored {
                let Some(source) = set.source(&path) else {
                    continue;
                };
                if let Some(git) = nested_git(&checkout.path, &path) {
                    kept.push(KeptPath {
                        checkout: checkout.name.clone(),
                        reason: format!(
                            "it holds the repository {git}, which may hold commits that exist nowhere else"
                        ),
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
            let removed = remove_tree(&path);
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
        if let Some(git) = nested_git(&checkout.path, path) {
            return Ok(Some(format!(
                "it holds the repository {git}, which may hold commits that exist nowhere else"
            )));
        }
        if self.backend.tracks(&checkout.path, path)? {
            return Ok(Some("the repository tracks a file in it".to_owned()));
        }
        Ok(None)
    }
}
