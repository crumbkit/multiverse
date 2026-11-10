//! The empty file `.deadband-workspace` that mxv keeps in each workspace's
//! directory. deadband shows a workspace name beside each job, and for a
//! job that names no workspace itself, such as `deadband run -- cargo test`
//! in a checkout, it walks up from the job's directory to the nearest
//! directory that holds a regular file of this name and takes that
//! directory's name. The file holds no bytes. mxv never replaces or deletes
//! an entry of this name that it did not write.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::store;

#[cfg(test)]
mod tests;

/// The name of the marker in a workspace's directory.
pub const MARKER_FILE: &str = ".deadband-workspace";

/// The permission bits of a marker that mxv writes.
const MARKER_MODE: u32 = 0o644;

/// What [`ensure`] found or did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marker {
    /// The directory had no marker, and [`ensure`] wrote one.
    Written,
    /// A regular file of the marker's name was already there, and stays as
    /// it is.
    Present,
    /// An entry of the marker's name that is not a regular file, such as a
    /// directory or a symbolic link, is at this path. It stays untouched,
    /// and deadband ignores it.
    Blocked(PathBuf),
}

/// Makes sure that the directory `dir` holds the marker: it writes an empty
/// regular file named [`MARKER_FILE`] with mode 0644 when nothing has the
/// name, and changes nothing otherwise.
///
/// # Errors
///
/// Returns [`Error::StateUnreadable`] when the entry cannot be read, and
/// [`Error::Unwritable`] when the marker cannot be written.
pub fn ensure(dir: &Path) -> Result<Marker, Error> {
    let path = dir.join(MARKER_FILE);
    if store::create_empty(&path, MARKER_MODE)? {
        return Ok(Marker::Written);
    }
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => Ok(Marker::Present),
        Ok(_) => Ok(Marker::Blocked(path)),
        Err(source) => Err(Error::StateUnreadable { path, source }),
    }
}

/// Deletes the marker in the directory `dir` when it is an empty regular
/// file, which is what mxv writes. Any other entry of the name stays.
///
/// # Errors
///
/// Returns [`Error::Unwritable`] when the marker cannot be deleted.
pub(crate) fn delete(dir: &Path) -> Result<(), Error> {
    let path = dir.join(MARKER_FILE);
    if !is_own(&path) {
        return Ok(());
    }
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Unwritable { path, source }),
    }
}

/// Reports whether the entry at `path` is a marker that mxv wrote: an empty
/// regular file.
pub(crate) fn is_own(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() == 0)
}
