//! The ignored files of a workspace, such as `.env`, that `mxv archive`
//! keeps in a private tar file and `mxv restore` writes back.
//!
//! The tar file is `ignored.tar` in the archive's data directory. The file
//! has the permission bits 0600 and its directory 0700, so only the person
//! who ran mxv can read them, because the files can hold secrets. Each tar
//! entry holds a path relative to the workspace's directory, the bytes of a
//! file or the target of a symbolic link, and the permission bits. The tar
//! is not compressed. The archive record lists each entry as an
//! [`IgnoredEntry`], with the SHA-256 digest of its bytes, so a compare
//! after the checkouts move aside and a restore check the entries without
//! trusting the tar.
//!
//! [`keep`] writes the tar in one step through the store, so a run that
//! stops partway leaves only a temporary file, which the next run removes.
//! [`changed`] names each original that differs from its entry. [`restore`]
//! checks every entry against the tar before it writes any of them, and
//! never writes over a path that the rebuilt checkout already holds.

use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::error::Error;
use crate::store;

#[cfg(test)]
mod tests;

/// The name of the tar file in an archive's data directory.
pub(crate) const IGNORED_TAR: &str = "ignored.tar";

/// The permission bits of the tar file.
const TAR_MODE: u32 = 0o600;

/// What one kept ignored entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IgnoredKind {
    /// A regular file, whose bytes the tar holds.
    File,
    /// A symbolic link, whose target the tar holds.
    Link,
}

/// One ignored file or symbolic link that an archive keeps. The record
/// names it and holds none of its contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct IgnoredEntry {
    /// The entry's path, relative to the workspace's directory, such as
    /// `app/.env`.
    pub path: String,
    /// Whether the entry is a file or a link.
    pub kind: IgnoredKind,
    /// The number of bytes of a file, or of a link's target.
    pub size: u64,
    /// The permission bits.
    pub mode: u32,
    /// The SHA-256 digest of a file's bytes, or of a link's target, in
    /// lowercase hex.
    pub digest: String,
}

impl IgnoredEntry {
    /// Returns the folder of the checkout that held the entry: the first
    /// name of its path, such as `app` of `app/.env`.
    #[must_use]
    pub fn folder(&self) -> &str {
        self.path.split('/').next().unwrap_or_default()
    }
}

/// One ignored file or symbolic link of a checkout that an archive plans
/// to keep.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct IgnoredSource {
    /// The checkout's folder.
    pub folder: String,
    /// The entry's path, relative to the checkout, such as `.env`.
    pub path: String,
    /// The number of bytes of a file, or 0 for a link.
    pub bytes: u64,
}

impl IgnoredSource {
    /// Returns the entry at `path` in the checkout `folder`, of `bytes`.
    #[must_use]
    pub const fn new(folder: String, path: String, bytes: u64) -> Self {
        Self {
            folder,
            path,
            bytes,
        }
    }

    /// Returns the path under which the archive keeps the entry, relative
    /// to the workspace's directory, such as `app/.env`.
    #[must_use]
    pub fn archived(&self) -> String {
        format!("{}/{}", self.folder, self.path)
    }
}

/// Returns each of `sources`, whose checkout is at `dir`, as its archived
/// path and where it lies on the disk.
pub(crate) fn on_disk(sources: &[IgnoredSource], dir: &Path) -> Vec<(String, PathBuf)> {
    sources
        .iter()
        .map(|source| (source.archived(), dir.join(&source.path)))
        .collect()
}

/// Why a restore cannot write the kept ignored entries back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IgnoredRefusal {
    /// These entries are missing from the tar, differ from their digest, or
    /// name a path outside the workspace's directory.
    Damaged(Vec<String>),
    /// The rebuilt workspace already holds something at these paths, or at
    /// a directory above them, so a restore would write over it or through
    /// a link.
    Taken(Vec<String>),
}

/// What a restore wrote back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct IgnoredRestored {
    /// The path of each entry written back.
    pub(crate) written: Vec<String>,
    /// The path of each damaged entry that a forced restore left out.
    pub(crate) lost: Vec<String>,
}

/// Writes each of `sources`, a path relative to the workspace's directory
/// with where the entry lies on the disk, into the tar file in `dir`, and
/// returns the entries in the order of `sources`. It makes `dir` with the
/// permission bits 0700 when it does not exist, and replaces a tar that an
/// earlier run wrote. The digests come from the bytes written into the tar.
///
/// # Errors
///
/// Returns [`Error::StateUnreadable`] when a source cannot be read, is not
/// a file or a link, or changes size while it is read, and
/// [`Error::Unwritable`] when the tar cannot be written.
#[cfg(test)]
pub(crate) fn keep(dir: &Path, sources: &[(String, PathBuf)]) -> Result<Vec<IgnoredEntry>, Error> {
    keep_carrying(dir, sources, &[])
}

/// Writes each of `sources` into the tar file in `dir`, as [`keep`] does,
/// and carries over each of `carried` that the tar an earlier run wrote in
/// `dir` still holds whole, unless a source already has its path. A run
/// that finishes an archive uses it after a checkout is retired, when the
/// checkout's files exist only in the tar. A carried entry that the old tar
/// no longer holds whole is left out, and a restore names it as damaged.
/// Returns the sources' entries, then the carried ones.
///
/// # Errors
///
/// Returns the errors of [`keep`].
pub(crate) fn keep_carrying(
    dir: &Path,
    sources: &[(String, PathBuf)],
    carried: &[IgnoredEntry],
) -> Result<Vec<IgnoredEntry>, Error> {
    store::private_dir(dir)?;
    let tar = dir.join(IGNORED_TAR);
    let carried: Vec<IgnoredEntry> = carried
        .iter()
        .filter(|entry| !sources.iter().any(|(path, _)| *path == entry.path))
        .cloned()
        .collect();
    let whole = whole_entries(&tar, &carried)?;
    let carried: Vec<IgnoredEntry> = carried
        .into_iter()
        .filter(|entry| whole.contains(&entry.path))
        .collect();
    let mut kept = Vec::with_capacity(sources.len() + carried.len());
    store::write_file_with(&tar, TAR_MODE, |file| {
        let unwritable = |source| Error::Unwritable {
            path: tar.clone(),
            source,
        };
        let mut builder = tar::Builder::new(file);
        for (path, on_disk) in sources {
            kept.push(append(&mut builder, path, on_disk)?);
        }
        each_entry(&tar, &carried, |entry, found| {
            if kept.iter().any(|copied| copied.path == entry.path) {
                // The old tar holds the path twice, and one copy suffices.
                return Ok(());
            }
            let mut header = found.header().clone();
            match entry.kind {
                IgnoredKind::Link => {
                    let target = found
                        .link_name()
                        .ok()
                        .flatten()
                        .ok_or_else(|| unwritable(io::ErrorKind::InvalidData.into()))?;
                    builder
                        .append_link(&mut header, &entry.path, &target)
                        .map_err(unwritable)?;
                }
                IgnoredKind::File => {
                    builder
                        .append_data(&mut header, &entry.path, &mut *found)
                        .map_err(unwritable)?;
                }
            }
            kept.push(entry.clone());
            Ok(())
        })?;
        builder.into_inner().map(|_| ()).map_err(unwritable)
    })?;
    Ok(kept)
}

/// Appends the entry at `on_disk` to `builder` under `path`, and returns
/// its record.
fn append(
    builder: &mut tar::Builder<&mut File>,
    path: &str,
    on_disk: &Path,
) -> Result<IgnoredEntry, Error> {
    let unreadable = |source| Error::StateUnreadable {
        path: on_disk.to_path_buf(),
        source,
    };
    let unwritable = |source| Error::Unwritable {
        path: on_disk.to_path_buf(),
        source,
    };
    let metadata = fs::symlink_metadata(on_disk).map_err(unreadable)?;
    let mode = metadata.permissions().mode() & 0o7777;
    let mut header = tar::Header::new_gnu();
    header.set_mode(mode);
    header.set_mtime(0);
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(on_disk).map_err(unreadable)?;
        let bytes = target.as_os_str().as_bytes();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        builder
            .append_link(&mut header, path, &target)
            .map_err(unwritable)?;
        return Ok(IgnoredEntry {
            path: path.to_owned(),
            kind: IgnoredKind::Link,
            size: len(bytes),
            mode,
            digest: store::hex_digest(&Sha256::digest(bytes)),
        });
    }
    let file = File::open(on_disk).map_err(unreadable)?;
    let opened = file.metadata().map_err(unreadable)?;
    if !opened.is_file() {
        return Err(unreadable(io::Error::new(
            io::ErrorKind::InvalidInput,
            "an ignored entry that an archive keeps is a file or a symbolic link",
        )));
    }
    let size = opened.len();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(size);
    let mut reader = Hashing::new(file.take(size));
    builder
        .append_data(&mut header, path, &mut reader)
        .map_err(unwritable)?;
    if reader.count != size {
        // A file that shrank while it was read would leave the tar short.
        return Err(unreadable(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the file changed size while the archive read it",
        )));
    }
    Ok(IgnoredEntry {
        path: path.to_owned(),
        kind: IgnoredKind::File,
        size,
        mode,
        digest: reader.digest(),
    })
}

/// Returns the length of `bytes` as a size.
fn len(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).unwrap_or(u64::MAX)
}

/// A reader that takes the SHA-256 digest of the bytes it passes on.
struct Hashing<R> {
    inner: R,
    hasher: Sha256,
    count: u64,
}

impl<R: Read> Hashing<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            count: 0,
        }
    }

    fn digest(self) -> String {
        store::hex_digest(&self.hasher.finalize())
    }
}

impl<R> std::fmt::Debug for Hashing<R> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Hashing")
            .field("count", &self.count)
            .finish_non_exhaustive()
    }
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        let read = buffer.get(..count).unwrap_or_default();
        self.hasher.update(read);
        self.count = self.count.saturating_add(len(read));
        Ok(count)
    }
}

/// Returns the path of each of `kept` whose original, among `sources`,
/// differs from it in kind, permission bits or bytes, or is gone, in the
/// order of `kept`. An archive refuses rather than delete such a file.
///
/// # Errors
///
/// Returns [`Error::StateUnreadable`] when an original cannot be read.
// Soft limit: the compare after a checkout moves aside does not call this
// yet; the retire step that compares the moved checkout's ignored files
// with the tar replaces this attribute.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the quarantine compare does not call it yet")
)]
pub(crate) fn changed(
    kept: &[IgnoredEntry],
    sources: &[(String, PathBuf)],
) -> Result<Vec<String>, Error> {
    let mut found = Vec::new();
    for entry in kept {
        let Some((_, on_disk)) = sources.iter().find(|(path, _)| *path == entry.path) else {
            found.push(entry.path.clone());
            continue;
        };
        if !matches(entry, on_disk)? {
            found.push(entry.path.clone());
        }
    }
    Ok(found)
}

/// Reports whether the entry at `on_disk` is still what `entry` records.
fn matches(entry: &IgnoredEntry, on_disk: &Path) -> Result<bool, Error> {
    let metadata = match fs::symlink_metadata(on_disk) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(Error::StateUnreadable {
                path: on_disk.to_path_buf(),
                source,
            });
        }
    };
    if metadata.permissions().mode() & 0o7777 != entry.mode {
        return Ok(false);
    }
    let digest = match entry.kind {
        IgnoredKind::Link if metadata.file_type().is_symlink() => {
            let target = fs::read_link(on_disk).map_err(|source| Error::StateUnreadable {
                path: on_disk.to_path_buf(),
                source,
            })?;
            store::hex_digest(&Sha256::digest(target.as_os_str().as_bytes()))
        }
        IgnoredKind::File if metadata.is_file() => store::file_digest(on_disk)?,
        IgnoredKind::File | IgnoredKind::Link => return Ok(false),
    };
    Ok(digest == entry.digest)
}

/// Writes each of `kept` from the tar file in `dir` back below the
/// workspace's directory `root`, with its permission bits, and makes the
/// directories above it that do not exist. It checks every entry against
/// the tar and the rebuilt workspace before it writes any. It refuses when
/// the rebuilt workspace already holds a path, and when an entry is
/// damaged, unless `force` is set, in which case it writes the whole
/// entries and names the damaged ones as lost.
///
/// # Errors
///
/// Returns [`Error::StateUnreadable`] when the tar cannot be read, and
/// [`Error::Unwritable`] when an entry cannot be written.
// Soft limit: `mxv restore` does not call this yet; the restore step that
// writes the ignored files back replaces this attribute.
#[cfg_attr(not(test), expect(dead_code, reason = "restore does not call it yet"))]
pub(crate) fn restore(
    dir: &Path,
    kept: &[IgnoredEntry],
    root: &Path,
    force: bool,
) -> Result<Result<IgnoredRestored, IgnoredRefusal>, Error> {
    let tar = dir.join(IGNORED_TAR);
    let whole = whole_entries(&tar, kept)?;
    let mut damaged = Vec::new();
    let mut taken = Vec::new();
    let mut ready = Vec::new();
    for entry in kept {
        let Some(relative) = safe_relative(&entry.path) else {
            damaged.push(entry.path.clone());
            continue;
        };
        if is_taken(root, &relative) {
            taken.push(entry.path.clone());
        } else if whole.contains(&entry.path) {
            ready.push(entry);
        } else {
            damaged.push(entry.path.clone());
        }
    }
    if !taken.is_empty() {
        return Ok(Err(IgnoredRefusal::Taken(taken)));
    }
    if !damaged.is_empty() && !force {
        return Ok(Err(IgnoredRefusal::Damaged(damaged)));
    }
    write_back(&tar, &ready, root)?;
    Ok(Ok(IgnoredRestored {
        written: ready.iter().map(|entry| entry.path.clone()).collect(),
        lost: damaged,
    }))
}

/// Calls `visit` with each entry of the tar at `tar` that one of `kept`
/// names, and that entry of `kept`. A missing tar has no entries, and a tar
/// cut short ends at the first entry that cannot be read.
fn each_entry(
    tar: &Path,
    kept: &[IgnoredEntry],
    mut visit: impl FnMut(&IgnoredEntry, &mut tar::Entry<'_, File>) -> Result<(), Error>,
) -> Result<(), Error> {
    let file = match File::open(tar) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(Error::StateUnreadable {
                path: tar.to_path_buf(),
                source,
            });
        }
    };
    let mut archive = tar::Archive::new(file);
    let Ok(entries) = archive.entries() else {
        return Ok(());
    };
    for found in entries {
        let Ok(mut found) = found else {
            break;
        };
        let path = found.path_bytes();
        let Some(entry) = kept
            .iter()
            .find(|entry| entry.path.as_bytes() == path.as_ref())
        else {
            continue;
        };
        visit(entry, &mut found)?;
    }
    Ok(())
}

/// Returns the path of each of `kept` whose entry in the tar at `tar` has
/// the recorded kind, size and digest. It streams each file's bytes, so a
/// large file is never held in memory.
fn whole_entries(
    tar: &Path,
    kept: &[IgnoredEntry],
) -> Result<std::collections::HashSet<String>, Error> {
    let mut whole = std::collections::HashSet::new();
    each_entry(tar, kept, |entry, found| {
        let is_whole = match (entry.kind, found.header().entry_type()) {
            (IgnoredKind::Link, tar::EntryType::Symlink) => {
                found.link_name().ok().flatten().is_some_and(|target| {
                    let bytes = target.as_os_str().as_bytes();
                    len(bytes) == entry.size
                        && store::hex_digest(&Sha256::digest(bytes)) == entry.digest
                })
            }
            (IgnoredKind::File, tar::EntryType::Regular) => {
                // One byte past the recorded size shows a longer entry
                // without reading all of it.
                let mut reader = Hashing::new((&mut *found).take(entry.size.saturating_add(1)));
                let read = io::copy(&mut reader, &mut io::sink());
                read.is_ok() && reader.count == entry.size && reader.digest() == entry.digest
            }
            _ => false,
        };
        if is_whole {
            whole.insert(entry.path.clone());
        }
        Ok(())
    })?;
    Ok(whole)
}

/// Writes each of `ready`, whose entries in the tar at `tar` are whole,
/// below `root`.
fn write_back(tar: &Path, ready: &[&IgnoredEntry], root: &Path) -> Result<(), Error> {
    let mut left: Vec<&str> = ready.iter().map(|entry| entry.path.as_str()).collect();
    each_entry(tar, kept_of(ready).as_slice(), |entry, found| {
        let Some(at) = left.iter().position(|path| *path == entry.path) else {
            return Ok(());
        };
        left.swap_remove(at);
        write_entry(entry, &root.join(&entry.path), found)
    })?;
    if let Some(path) = left.first() {
        // The tar changed between the check and the write.
        return Err(Error::StateUnreadable {
            path: tar.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("the tar no longer holds {path}"),
            ),
        });
    }
    Ok(())
}

/// Returns a copy of each of `ready`.
fn kept_of(ready: &[&IgnoredEntry]) -> Vec<IgnoredEntry> {
    ready.iter().map(|entry| (*entry).clone()).collect()
}

/// Returns `path` as a relative path of plain names, or `None` when it is
/// empty, absolute, or holds `.` or `..`, which could leave the workspace.
fn safe_relative(path: &str) -> Option<PathBuf> {
    let relative = Path::new(path);
    let plain = relative
        .components()
        .all(|component| matches!(component, Component::Normal(_)));
    (plain && !path.is_empty() && !path.ends_with('/')).then(|| relative.to_path_buf())
}

/// Reports whether `root` already holds something at `relative`, or holds
/// something other than a directory at a directory above it, such as a
/// file or a symbolic link that a write would follow.
fn is_taken(root: &Path, relative: &Path) -> bool {
    let mut at = root.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        at.push(component);
        let last = components.peek().is_none();
        match fs::symlink_metadata(&at) {
            Ok(_) if last => return true,
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return false,
            Err(_) => return true,
        }
    }
    false
}

/// Writes the tar's entry `found` at `to` as `entry` records it, with its
/// permission bits.
fn write_entry(
    entry: &IgnoredEntry,
    to: &Path,
    found: &mut tar::Entry<'_, File>,
) -> Result<(), Error> {
    let unwritable = |source| Error::Unwritable {
        path: to.to_path_buf(),
        source,
    };
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(unwritable)?;
    }
    match entry.kind {
        IgnoredKind::File => {
            let digest = store::write_from(&mut found.take(entry.size), to, to, entry.mode)?;
            if digest != entry.digest {
                let _ = fs::remove_file(to);
                return Err(unwritable(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the tar's entry changed between the check and the write",
                )));
            }
        }
        IgnoredKind::Link => {
            let target = found
                .link_name()
                .ok()
                .flatten()
                .ok_or_else(|| unwritable(io::Error::from(io::ErrorKind::InvalidData)))?;
            std::os::unix::fs::symlink(&target, to).map_err(unwritable)?;
        }
    }
    Ok(())
}
