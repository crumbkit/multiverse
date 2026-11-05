//! The copy-on-write clones of a repository's ignored directories, such as
//! `node_modules/`, that a new checkout receives.
//!
//! On macOS, a clone is one `clonefile(2)` call, which clones a whole
//! directory tree on APFS. The clone shares its blocks with the source
//! until either copy of a file changes, so it costs almost no disk at
//! first. When the file system cannot clone, such as HFS+ or a checkout on
//! another volume, the clone fails and nothing is copied. Other systems
//! clone nothing yet. A clone never falls back to a full copy.
//!
//! A clone copies symbolic links as they are. An absolute link inside a
//! cloned `node_modules/`, such as one that a package manager wrote to a
//! workspace package, still points into the repository's own checkout, so
//! the new checkout reads that package from the repository until the next
//! install rewrites the link.

use std::fs;
use std::io;
use std::path::Path;

use crate::backend::Backend;
use crate::disposable::{CLONE_NEVER, PathGlob};
use crate::error::Error;
use crate::record::{Checkout, Workspace};
use crate::workspaces::{Entry, Mode, Workspaces};

#[cfg(test)]
mod tests;

/// The words of a warning that the file system cannot clone, which a
/// caller can look for.
pub const UNSUPPORTED: &str = "cannot clone files";

/// What [`Workspaces::clone_ignored`] cloned, or would clone on a dry run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Cloned {
    /// Each directory that was cloned, with the checkout's name in front,
    /// such as `app: web/node_modules/`.
    pub paths: Vec<String>,
    /// Why the cloning stopped before every directory was cloned, or
    /// `None` when nothing stopped it.
    pub warning: Option<String>,
}

/// Clones the directory `from` to `to`, which does not exist, with
/// `clonefile(2)`.
#[cfg(target_os = "macos")]
fn clone_directory(from: &Path, to: &Path) -> io::Result<()> {
    reflink_copy::reflink(from, to)
}

/// Fails, because only macOS clones directories yet.
#[cfg(not(target_os = "macos"))]
fn clone_directory(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Reports whether `error` says that the file system cannot clone, such as
/// `ENOTSUP` from `clonefile(2)` on HFS+ or `EXDEV` across volumes.
fn cannot_clone(error: &io::Error) -> bool {
    #[cfg(target_os = "macos")]
    if error.raw_os_error() == Some(libc::ENOTSUP) {
        return true;
    }
    matches!(
        error.kind(),
        io::ErrorKind::Unsupported | io::ErrorKind::CrossesDevices
    )
}

/// Returns the warning of a clone of `path` into `checkout` that failed
/// with `error`.
fn warning(checkout: &Path, path: &str, error: &io::Error) -> String {
    if cannot_clone(error) {
        format!(
            "the file system of {} {UNSUPPORTED}, so mxv cloned no ignored directory and copied nothing",
            checkout.display()
        )
    } else {
        format!(
            "mxv stopped cloning ignored directories into {} when cloning {path} failed: {error}; mxv never copies them in full",
            checkout.display()
        )
    }
}

impl<B: Backend> Workspaces<B> {
    /// Clones into each checkout of `workspace` the ignored directories of
    /// its repository that `globs` match, such as `node_modules/`, as the
    /// module docs describe, and returns what it cloned. A dry run returns
    /// what a run would clone, and clones nothing.
    ///
    /// A directory is cloned only when `git status` in the repository
    /// reports it as a whole ignored directory, it is not a symbolic link,
    /// its name is not `target`, nothing is at its place in the checkout,
    /// and its parent directory exists there. The first clone that fails
    /// stops the cloning with a warning, and its partial copy is deleted.
    ///
    /// # Errors
    ///
    /// Returns the failure of `git status` in a repository.
    pub fn clone_ignored(
        &self,
        workspace: &Workspace,
        globs: &[PathGlob],
        mode: Mode,
    ) -> Result<Cloned, Error> {
        let each: Vec<(&Checkout, Vec<PathGlob>)> = workspace
            .checkouts
            .iter()
            .map(|checkout| (checkout, globs.to_vec()))
            .collect();
        self.clone_ignored_each(&each, mode)
    }

    /// Clones into each checkout of `each` the ignored directories of its
    /// repository that the checkout's own globs match, as
    /// [`Workspaces::clone_ignored`] does, for a workspace whose checkouts
    /// read different manifests.
    ///
    /// # Errors
    ///
    /// Returns the failure of `git status` in a repository.
    pub fn clone_ignored_each(
        &self,
        each: &[(&Checkout, Vec<PathGlob>)],
        mode: Mode,
    ) -> Result<Cloned, Error> {
        let mut cloned = Cloned::default();
        for (checkout, globs) in each {
            if globs.is_empty() {
                continue;
            }
            let changes = self.backend.changes(&checkout.source)?;
            for path in changes.ignored {
                let Some(body) = path.strip_suffix('/') else {
                    continue;
                };
                if !globs.iter().any(|glob| glob.matches(&path))
                    || body.rsplit('/').next() == Some(CLONE_NEVER)
                {
                    continue;
                }
                let from = checkout.source.join(body);
                let to = checkout.path.join(body);
                let is_directory = fs::symlink_metadata(&from)
                    .is_ok_and(|metadata| metadata.is_dir() && !metadata.is_symlink());
                if !is_directory {
                    continue;
                }
                let named = format!("{}: {path}", checkout.name);
                if mode == Mode::DryRun {
                    cloned.paths.push(named);
                    continue;
                }
                let parent_present = to.parent().is_some_and(Path::is_dir);
                if !parent_present || Entry::at(&to)? != Entry::Absent {
                    continue;
                }
                if let Err(error) = clone_directory(&from, &to) {
                    if Entry::at(&to)? != Entry::Absent {
                        let _ = fs::remove_dir_all(&to);
                    }
                    cloned.warning = Some(warning(&checkout.path, &path, &error));
                    return Ok(cloned);
                }
                cloned.paths.push(named);
            }
        }
        Ok(cloned)
    }
}
