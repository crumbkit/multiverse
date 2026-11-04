//! The seam between workspace operations and the version-control system
//! that makes each checkout. [`Git`](crate::Git) is the one backend.

use std::path::{Path, PathBuf};

use crate::error::Error;

/// A repository that a workspace can check out: its top-level directory
/// and the name that its checkout directory takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repository {
    /// The repository's top-level directory, as an absolute path.
    pub root: PathBuf,
    /// The last component of `root`, such as `inbox`, which names the
    /// repository's checkout inside a workspace.
    pub name: String,
}

/// The operations that a workspace needs from a version-control system.
///
/// Each method runs one operation in one repository or checkout, and
/// returns [`Error`] for a failure that the domain does not expect. A
/// backend that runs a program states the program, its arguments, its
/// environment, its streams and deadline, and who ends it, at its own
/// implementation.
pub trait Backend {
    /// Returns the repository that holds `path`, which has a remote named
    /// `origin`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RepositoryNotFound`] when `path` is not in a
    /// repository with a working tree, and [`Error::NoOrigin`] when the
    /// repository has no remote named `origin`.
    fn repository(&self, path: &Path) -> Result<Repository, Error>;

    /// Updates the repository's copy of `origin`.
    ///
    /// # Errors
    ///
    /// Returns the failure of the fetch.
    fn fetch(&self, repository: &Path) -> Result<(), Error>;

    /// Returns the remote branch that new work starts from, such as
    /// `origin/main`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::EmptyOrigin`] when the repository's copy of
    /// `origin` holds no branch, and [`Error::NoDefaultBranch`] when it
    /// holds branches but no default branch.
    fn base(&self, repository: &Path) -> Result<String, Error>;

    /// Reports whether the repository has a local branch named `branch`.
    ///
    /// # Errors
    ///
    /// Returns the failure of the lookup.
    fn has_branch(&self, repository: &Path, branch: &str) -> Result<bool, Error>;

    /// Creates a checkout of the repository at `checkout`, on a new branch
    /// `branch` that starts at `base` and tracks nothing, and returns the
    /// commit that it starts at.
    ///
    /// # Errors
    ///
    /// Returns the failure of the command that creates the checkout.
    fn add_checkout(
        &self,
        repository: &Path,
        checkout: &Path,
        branch: &str,
        base: &str,
    ) -> Result<String, Error>;

    /// Reports whether the repository lists `checkout` as one of its
    /// checkouts.
    ///
    /// # Errors
    ///
    /// Returns the failure of the listing.
    fn owns_checkout(&self, repository: &Path, checkout: &Path) -> Result<bool, Error>;

    /// Returns one line for each uncommitted or untracked change in
    /// `checkout`, such as `?? notes.md`, and no lines for a clean
    /// checkout.
    ///
    /// # Errors
    ///
    /// Returns the failure of the status command.
    fn changes(&self, checkout: &Path) -> Result<Vec<String>, Error>;

    /// Removes the checkout at `checkout` from the repository and deletes
    /// its directory. With `force`, it also deletes uncommitted and
    /// untracked changes.
    ///
    /// # Errors
    ///
    /// Returns the failure of the removal.
    fn remove_checkout(&self, repository: &Path, checkout: &Path, force: bool)
    -> Result<(), Error>;

    /// Forgets the repository's checkouts whose directories are gone.
    ///
    /// # Errors
    ///
    /// Returns the failure of the prune.
    fn prune_checkouts(&self, repository: &Path) -> Result<(), Error>;

    /// Reports whether every commit of `branch` is already on `base`.
    ///
    /// # Errors
    ///
    /// Returns the failure of the comparison.
    fn is_merged(&self, repository: &Path, branch: &str, base: &str) -> Result<bool, Error>;

    /// Deletes the local branch `branch`, merged or not.
    ///
    /// # Errors
    ///
    /// Returns the failure of the deletion.
    fn delete_branch(&self, repository: &Path, branch: &str) -> Result<(), Error>;
}
