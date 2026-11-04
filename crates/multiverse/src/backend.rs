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
    /// The last component of `root`, such as `app`, which names the
    /// repository's checkout inside a workspace.
    pub name: String,
}

/// The changes in a checkout that removing it would delete.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Changes {
    /// One line for each uncommitted change or untracked path, as
    /// `git status --porcelain` prints it, such as `?? notes.md`.
    pub changed: Vec<String>,
    /// Each path that the repository's ignore rules ignore, such as
    /// `target/` or `.env`. An ignored directory is one path.
    pub ignored: Vec<String>,
}

/// A checkout as its repository lists it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Listed {
    /// The commit that the checkout's `HEAD` names, or `None` for a
    /// checkout whose `HEAD` names no commit yet.
    pub head: Option<String>,
    /// Whether the checkout's `HEAD` is detached from every branch.
    pub detached: bool,
    /// The lock of the checkout: `None` when it is unlocked, and the lock's
    /// reason otherwise, which is empty when the lock gives none. `git
    /// worktree add` locks a new checkout with the reason
    /// [`INITIALIZING`] until it finishes, so a killed run leaves that lock.
    pub locked: Option<String>,
}

/// The lock reason that `git worktree add` gives a checkout while it makes
/// it.
pub const INITIALIZING: &str = "initializing";

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

    /// Returns the remote branch that new work starts from, as a full
    /// reference such as `refs/remotes/origin/main`.
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

    /// Creates the local branch `branch` at the commit that `base` names,
    /// in one step that fails when the branch already exists, so a branch
    /// that another process created is never taken over. Returns the commit
    /// when it created the branch, and `None` when the branch already
    /// existed.
    ///
    /// # Errors
    ///
    /// Returns the failure of the lookup of `base` or of the creation.
    fn create_branch(
        &self,
        repository: &Path,
        branch: &str,
        base: &str,
    ) -> Result<Option<String>, Error>;

    /// Creates a checkout of the repository at `checkout` on the existing
    /// branch `branch`, and returns the commit that it starts at.
    ///
    /// # Errors
    ///
    /// Returns the failure of the command that creates the checkout.
    fn add_checkout(
        &self,
        repository: &Path,
        checkout: &Path,
        branch: &str,
    ) -> Result<String, Error>;

    /// Returns the checkout at `checkout` as the repository lists it, or
    /// `None` when the repository does not list it. A checkout whose
    /// directory is gone stays listed until it is pruned.
    ///
    /// # Errors
    ///
    /// Returns the failure of the listing.
    fn listed_checkout(&self, repository: &Path, checkout: &Path) -> Result<Option<Listed>, Error>;

    /// Unlocks the checkout at `checkout`, so it can be removed or pruned.
    ///
    /// # Errors
    ///
    /// Returns the failure of the unlock.
    fn unlock_checkout(&self, repository: &Path, checkout: &Path) -> Result<(), Error>;

    /// Returns the uncommitted changes, untracked paths and ignored paths
    /// in `checkout`.
    ///
    /// # Errors
    ///
    /// Returns the failure of the status command.
    fn changes(&self, checkout: &Path) -> Result<Changes, Error>;

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

    /// Returns the number of commits that `revision`, such as
    /// `refs/heads/reading-list`, holds and `base` does not, or `None` when
    /// `base` names no commit any more, such as a remote branch that was
    /// deleted, so the count is unknown.
    ///
    /// # Errors
    ///
    /// Returns the failure of the count.
    fn unmerged_commits(
        &self,
        repository: &Path,
        revision: &str,
        base: &str,
    ) -> Result<Option<u32>, Error>;

    /// Returns the number of commits that `revision` holds and no branch,
    /// tag or remote branch of the repository at `dir` holds, such as
    /// commits made on a detached `HEAD`. `dir` is a checkout with
    /// `revision` `HEAD`, or the repository with the commit that a
    /// checkout's `HEAD` names. Removing the checkout loses them.
    ///
    /// # Errors
    ///
    /// Returns the failure of the count.
    fn stray_commits(&self, dir: &Path, revision: &str) -> Result<u32, Error>;

    /// Deletes the local branch `branch` only while it still points at
    /// `commit`, so a branch that moved since is kept.
    ///
    /// # Errors
    ///
    /// Returns the failure of the deletion, such as when the branch moved.
    fn delete_branch_at(&self, repository: &Path, branch: &str, commit: &str) -> Result<(), Error>;

    /// Deletes the local branch `branch`, merged or not.
    ///
    /// # Errors
    ///
    /// Returns the failure of the deletion.
    fn delete_branch(&self, repository: &Path, branch: &str) -> Result<(), Error>;
}
