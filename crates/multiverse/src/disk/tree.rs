//! The checks and the deletion of a disposable path, which work by file
//! descriptor from the checkout's root.
//!
//! Each function opens the checkout's root once, then opens each component
//! of the path relative to the one before it with `O_NOFOLLOW`, so a
//! component that another process swaps for a symbolic link is never
//! followed. Below the path, each directory is opened relative to its
//! parent in the same way, and each entry is deleted with `unlinkat`
//! relative to its directory, as `std::fs::remove_dir_all` does. A path
//! longer than `PATH_MAX` is never built, so a deep tree is read in full.
//!
//! A tree may not cross into another file system, such as a disk image
//! mounted below `target/`, and may not be deeper than [`MOST_DEPTH`]
//! directories below the path. [`keep_reason`] finds both before anything
//! is deleted, and [`remove`] refuses both again during the deletion.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, CWD, Dev, Dir, FileType, Mode, OFlags, Stat, openat, statat, unlinkat};
use rustix::io::Errno;

/// The most directories below a disposable path that mxv checks and
/// deletes. A deeper tree is kept, and each directory open holds one file
/// descriptor, so the limit also bounds the descriptors that a walk holds.
pub(crate) const MOST_DEPTH: usize = 100;

/// A path that could not be read or deleted, and why.
pub(crate) type Failure = (PathBuf, io::Error);

/// The flags that open a directory without following a symbolic link.
fn directory_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

/// Opens the parent of `relative` below the checkout at `root`, one
/// component at a time without following a symbolic link, and returns it
/// with the last component's name.
fn open_parent(root: &Path, relative: &Path) -> Result<(OwnedFd, OsString), Failure> {
    let mut names: Vec<&OsStr> = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(name) => names.push(name),
            _ => {
                return Err((
                    root.join(relative),
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "the path must be relative to the checkout and hold only names",
                    ),
                ));
            }
        }
    }
    let Some((last, parents)) = names.split_last() else {
        return Err((root.to_path_buf(), io::ErrorKind::InvalidInput.into()));
    };
    let mut at = root.to_path_buf();
    let mut fd = openat(CWD, root, directory_flags(), Mode::empty())
        .map_err(|error| (at.clone(), io::Error::from(error)))?;
    for name in parents {
        at.push(name);
        fd = openat(&fd, *name, directory_flags(), Mode::empty())
            .map_err(|error| (at.clone(), io::Error::from(error)))?;
    }
    Ok((fd, last.to_os_string()))
}

/// Returns the names of the entries of the open directory `fd`, without
/// `.` and `..`. The listing's own descriptor is closed when it returns.
fn names(fd: &OwnedFd) -> Result<Vec<OsString>, Errno> {
    let mut names = Vec::new();
    for entry in Dir::read_from(fd)? {
        let bytes = entry?.file_name().to_bytes().to_vec();
        if bytes != b"." && bytes != b".." {
            names.push(OsStr::from_bytes(&bytes).to_os_string());
        }
    }
    Ok(names)
}

/// Reports whether the entry of `stat` is a directory.
fn is_directory(stat: &Stat) -> bool {
    FileType::from_raw_mode(stat.st_mode) == FileType::Directory
}

/// Returns the device of the entry of `stat`.
fn device(stat: &Stat) -> Dev {
    stat.st_dev
}

/// Returns why mxv keeps the disposable path `relative` of the checkout at
/// `root`, or `None` when it may be deleted. A directory is kept when it
/// holds a nested repository, an entry named `.git` in any case or a
/// directory that holds `HEAD`, `objects/` and `refs/` as a bare repository
/// does, at any depth. It is also kept when it holds the mount point of
/// another file system, when it is deeper than [`MOST_DEPTH`], and when mxv
/// cannot read all of it, because then mxv cannot tell what it holds. A
/// file is never kept here.
pub(crate) fn keep_reason(root: &Path, relative: &str) -> Option<String> {
    keep_reason_on(root, relative, None)
}

/// Returns what [`keep_reason`] returns, with `device` in place of the
/// device of the path, so a test can treat every directory as a mount
/// point.
pub(crate) fn keep_reason_on(
    root: &Path,
    relative: &str,
    device_of_root: Option<Dev>,
) -> Option<String> {
    let body = relative.strip_suffix('/')?;
    let path = Path::new(body);
    let (parent, name) = match open_parent(root, path) {
        Ok(opened) => opened,
        Err((at, error)) => {
            return Some(format!("mxv cannot open {}: {error}", shown(root, &at)));
        }
    };
    let stat = match statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(error) => return Some(format!("mxv cannot read {body}: {error}")),
    };
    if !is_directory(&stat) {
        return None;
    }
    let root_device = device_of_root.unwrap_or_else(|| device(&stat));
    scan(&parent, &name, path, root_device, 0)
}

/// Returns `path`, which lies below `root`, relative to `root`.
fn shown(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Returns why the directory `name` of the open directory `parent`, at the
/// path `path` relative to the checkout, is kept, as [`keep_reason`]
/// describes, where `depth` counts the directories above it below the
/// disposable path.
fn scan(
    parent: &OwnedFd,
    name: &OsStr,
    path: &Path,
    root_device: Dev,
    depth: usize,
) -> Option<String> {
    let at = path.display();
    if depth > MOST_DEPTH {
        return Some(format!(
            "it is deeper than {MOST_DEPTH} directories at {at}, so mxv cannot check all of it"
        ));
    }
    let unreadable = |error: Errno| {
        Some(format!(
            "mxv cannot read {at}: {error}, so it cannot tell whether it holds a repository"
        ))
    };
    let fd = match openat(parent, name, directory_flags(), Mode::empty()) {
        Ok(fd) => fd,
        Err(error) => return unreadable(error),
    };
    let entries = match names(&fd) {
        Ok(entries) => entries,
        Err(error) => return unreadable(error),
    };
    let kind = |entry: &str| statat(&fd, entry, AtFlags::SYMLINK_NOFOLLOW).ok();
    let bare = kind("HEAD")
        .is_some_and(|stat| FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile)
        && kind("objects").is_some_and(|stat| is_directory(&stat))
        && kind("refs").is_some_and(|stat| is_directory(&stat));
    if bare {
        return Some(format!(
            "it holds the repository {at}/, which may hold commits that exist nowhere else"
        ));
    }
    for entry in &entries {
        let child = path.join(entry);
        if entry.eq_ignore_ascii_case(".git") {
            return Some(format!(
                "it holds the repository {}, which may hold commits that exist nowhere else",
                child.display()
            ));
        }
        let stat = match statat(&fd, entry, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(error) => {
                return Some(format!(
                    "mxv cannot read {}: {error}, so it cannot tell whether it holds a repository",
                    child.display()
                ));
            }
        };
        if !is_directory(&stat) {
            continue;
        }
        if device(&stat) != root_device {
            return Some(format!(
                "it holds {}, the mount point of another file system, which mxv leaves alone",
                child.display()
            ));
        }
        if let Some(reason) = scan(&fd, entry, &child, root_device, depth + 1) {
            return Some(reason);
        }
    }
    None
}

/// Deletes the disposable path `relative` of the checkout at `root` and
/// everything below it, as the module docs describe, and calls `removed`
/// with each entry it deleted. A failure below the path does not stop the
/// deletion of its other entries, and the result names the first entry
/// that could not be deleted.
pub(crate) fn remove(
    root: &Path,
    relative: &str,
    removed: &mut dyn FnMut(&Path),
) -> Result<(), Failure> {
    remove_on(root, relative, None, removed)
}

/// Deletes as [`remove`] does, with `device` in place of the device of the
/// path, so a test can treat every directory as a mount point.
pub(crate) fn remove_on(
    root: &Path,
    relative: &str,
    device_of_root: Option<Dev>,
    removed: &mut dyn FnMut(&Path),
) -> Result<(), Failure> {
    let body = relative.trim_end_matches('/');
    let (parent, name) = open_parent(root, Path::new(body))?;
    let path = root.join(body);
    let stat = match statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(Errno::NOENT) => return Ok(()),
        Err(error) => return Err((path, io::Error::from(error))),
    };
    let root_device = device_of_root.unwrap_or_else(|| device(&stat));
    let mut walk = Removal {
        root_device,
        removed,
    };
    walk.entry(&parent, &name, &path, 0)
}

/// One deletion of a tree.
struct Removal<'a> {
    /// The device of the disposable path, which every directory below it
    /// must share.
    root_device: Dev,
    /// Called with each entry that the deletion removed.
    removed: &'a mut dyn FnMut(&Path),
}

impl Removal<'_> {
    /// Deletes the entry `name` of the open directory `dir`, whose path is
    /// `path`, and everything below it, where `depth` counts the
    /// directories above it below the disposable path.
    fn entry(
        &mut self,
        dir: &OwnedFd,
        name: &OsStr,
        path: &Path,
        depth: usize,
    ) -> Result<(), Failure> {
        let fail = |error: Errno| (path.to_path_buf(), io::Error::from(error));
        let stat = match statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return Ok(()),
            Err(error) => return Err(fail(error)),
        };
        if !is_directory(&stat) {
            return self.unlink(dir, name, AtFlags::empty(), path);
        }
        if device(&stat) != self.root_device {
            return Err((
                path.to_path_buf(),
                io::Error::other(
                    "it is the mount point of another file system, which mxv leaves alone",
                ),
            ));
        }
        if depth > MOST_DEPTH {
            return Err((
                path.to_path_buf(),
                io::Error::other(format!(
                    "it is deeper than {MOST_DEPTH} directories below the disposable path"
                )),
            ));
        }
        let child = match openat(dir, name, directory_flags(), Mode::empty()) {
            Ok(child) => child,
            // The entry stopped being a directory after the stat, such as a
            // directory that was swapped for a symbolic link, so only the
            // entry itself is deleted.
            Err(Errno::LOOP | Errno::NOTDIR) => {
                return self.unlink(dir, name, AtFlags::empty(), path);
            }
            Err(Errno::NOENT) => return Ok(()),
            Err(error) => return Err(fail(error)),
        };
        let mut first = None;
        for entry in names(&child).map_err(fail)? {
            if let Err(failure) = self.entry(&child, &entry, &path.join(&entry), depth + 1) {
                first.get_or_insert(failure);
            }
        }
        if let Some(failure) = first {
            return Err(failure);
        }
        self.unlink(dir, name, AtFlags::REMOVEDIR, path)
    }

    /// Deletes the entry `name` of the open directory `dir`, whose path is
    /// `path`, with `unlinkat` and `flags`.
    fn unlink(
        &mut self,
        dir: &OwnedFd,
        name: &OsStr,
        flags: AtFlags,
        path: &Path,
    ) -> Result<(), Failure> {
        match unlinkat(dir, name, flags) {
            Ok(()) => {
                (self.removed)(path);
                Ok(())
            }
            Err(Errno::NOENT) => Ok(()),
            Err(error) => Err((path.to_path_buf(), io::Error::from(error))),
        }
    }
}
