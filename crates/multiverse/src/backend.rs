//! The seam between workspace operations and the version-control system
//! that makes each checkout. [`Git`](crate::Git) is the one backend.
//!
//! A workspace made before clones holds git worktrees of the person's
//! repository, which the worktree methods make and remove. A newer
//! workspace holds clones of a bare mirror, which the mirror and clone
//! methods make.

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

/// What a checkout has checked out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Head {
    /// The top-level directory of the repository that git reads in the
    /// checkout, or `None` when git finds no repository there. It differs
    /// from the checkout when the checkout lost its `.git` inside another
    /// repository.
    pub toplevel: Option<PathBuf>,
    /// The branch that `HEAD` names, such as `triage`, or `None` for a
    /// detached `HEAD`.
    pub branch: Option<String>,
    /// The commit that `HEAD` names, or `None` before the first commit.
    pub commit: Option<String>,
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

    /// Returns the commit that the local branch `branch` points at, or
    /// `None` when the repository has no such branch.
    ///
    /// # Errors
    ///
    /// Returns the failure of the lookup.
    fn branch_tip(&self, repository: &Path, branch: &str) -> Result<Option<String>, Error>;

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

    /// Returns the branch and the commit that `checkout` has checked out.
    ///
    /// # Errors
    ///
    /// Returns the failure of the commands that read them.
    fn head(&self, checkout: &Path) -> Result<Head, Error>;

    /// Reports whether the repository tracks `path`, a path relative to
    /// `checkout` such as `target/`, or any file below it. A path that
    /// holds a tracked file is never disposable.
    ///
    /// # Errors
    ///
    /// Returns the failure of the listing.
    fn tracks(&self, checkout: &Path, path: &str) -> Result<bool, Error>;

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

    /// Returns the URL of the repository's remote named `origin`, as its
    /// config holds it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoOrigin`] when the repository has no `origin`.
    fn origin_url(&self, repository: &Path) -> Result<String, Error>;

    /// Makes `mirror` a bare repository when it is not one yet, and sets the
    /// config that keeps it from losing objects: `gc.auto=0`,
    /// `gc.pruneExpire=never` and `maintenance.auto=false`. A clone that
    /// borrows the mirror's objects breaks when the mirror loses one.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unwritable`] when the mirror's directory cannot be
    /// made, and the failure of the init or the config.
    fn prepare_mirror(&self, mirror: &Path) -> Result<(), Error>;

    /// Fetches every branch and tag of the origin at `url` into the bare
    /// `mirror`, deletes the mirror's branches that the origin no longer
    /// has, and points the mirror's `HEAD` at its branch `default`. The
    /// fetch uses the person's own git config, so a credential helper
    /// answers as it does for any other fetch.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MirrorFetchFailed`] when the fetch fails, and the
    /// failure of setting `HEAD`.
    fn fetch_mirror(&self, mirror: &Path, url: &str, default: &str) -> Result<(), Error>;

    /// Clones the bare `mirror` into `checkout` with `--shared` and without
    /// git's templates, so the clone borrows the mirror's objects and starts
    /// with no hooks, and checks out no files yet. It then sets the clone's
    /// `origin` to `url`, so a push or a fetch in the clone goes to the
    /// origin itself.
    ///
    /// # Errors
    ///
    /// Returns the failure of the clone or of setting the URL.
    fn clone_mirror(&self, mirror: &Path, checkout: &Path, url: &str) -> Result<(), Error>;

    /// Returns the path of the repository's `info/exclude` file, which holds
    /// the ignore rules that the person keeps outside `.gitignore`. The file
    /// need not exist.
    ///
    /// # Errors
    ///
    /// Returns the failure of the lookup.
    fn exclude_file(&self, repository: &Path) -> Result<PathBuf, Error>;

    /// Checks out the existing branch `branch` in the clone at `checkout`,
    /// writing its files, and returns the commit that it starts at.
    ///
    /// # Errors
    ///
    /// Returns the failure of the checkout.
    fn switch_branch(&self, checkout: &Path, branch: &str) -> Result<String, Error>;

    /// Updates the remote branches, and a copy of the tags and notes, of
    /// the clone at `checkout` and of each submodule it has initialised
    /// from their `origin`, and deletes those that the origin no longer
    /// has, so a commit counts as pushed only while the origin still holds
    /// it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::OriginFetchFailed`] when a fetch fails.
    fn refresh_origin(&self, checkout: &Path) -> Result<(), Error>;

    /// Returns the commits that the clone at `checkout`, or a submodule it
    /// has initialised, holds in its `HEAD`, branches, tags or notes, and
    /// that no branch, tag or note of `origin` holds, with the commits made
    /// on a detached `HEAD` that only the reflog of `HEAD` holds and whose
    /// patch no commit of `origin` or of a local branch has. Deleting the
    /// clone loses them.
    ///
    /// # Errors
    ///
    /// Returns the failure of the listing.
    fn unpushed_commits(&self, checkout: &Path) -> Result<Vec<String>, Error>;

    /// Returns the commit of each stash entry of the clone at `checkout`,
    /// newest first.
    ///
    /// # Errors
    ///
    /// Returns the failure of the listing.
    fn stashes(&self, checkout: &Path) -> Result<Vec<String>, Error>;

    /// Deletes the local branch `branch` only while it still points at
    /// `commit`, so a branch that moved since is kept.
    ///
    /// # Errors
    ///
    /// Returns the failure of the deletion, such as when the branch moved.
    fn delete_branch_at(&self, repository: &Path, branch: &str, commit: &str) -> Result<(), Error>;

    /// Writes the reference `reference` at `commit` in the repository at
    /// `repository`, replacing what it held. The reference keeps the commit
    /// from garbage collection.
    ///
    /// # Errors
    ///
    /// Returns the failure of the update.
    fn write_ref(&self, repository: &Path, reference: &str, commit: &str) -> Result<(), Error>;

    /// Fetches the `HEAD` of the clone at `checkout` into the bare `mirror`
    /// under the reference `reference`, and returns the commit that the
    /// reference then names.
    ///
    /// # Errors
    ///
    /// Returns the failure of the fetch.
    fn fetch_head_into_mirror(
        &self,
        mirror: &Path,
        checkout: &Path,
        reference: &str,
    ) -> Result<String, Error>;

    /// Deletes the reference `reference` of the repository at `repository`.
    /// Deleting a reference that does not exist succeeds.
    ///
    /// # Errors
    ///
    /// Returns the failure of the deletion.
    fn delete_ref(&self, repository: &Path, reference: &str) -> Result<(), Error>;

    /// Returns the commit that `revision` names in the repository at
    /// `repository`, or `None` when it names no commit there.
    ///
    /// # Errors
    ///
    /// Returns the failure of the lookup, such as a directory that is not a
    /// repository.
    fn resolve_commit(&self, repository: &Path, revision: &str) -> Result<Option<String>, Error>;

    /// Stages every change and untracked file of `checkout` that the ignore
    /// rules do not ignore, and commits them with `message`, without
    /// running any hook.
    ///
    /// # Errors
    ///
    /// Returns the failure of the staging or the commit.
    fn commit_all(&self, checkout: &Path, message: &str) -> Result<(), Error>;

    /// Returns the commits of `revision` that no remote branch of `origin`
    /// holds, as the checkout or repository at `dir` last fetched them.
    ///
    /// # Errors
    ///
    /// Returns the failure of the listing.
    fn local_only_commits(&self, dir: &Path, revision: &str) -> Result<Vec<String>, Error>;

    /// Returns the remote branch that `branch` of the checkout at
    /// `checkout` tracks, as a reference on the remote such as
    /// `refs/heads/main`, or `None` when it tracks none.
    ///
    /// # Errors
    ///
    /// Returns the failure of the lookup.
    fn upstream(&self, checkout: &Path, branch: &str) -> Result<Option<String>, Error>;

    /// Makes `branch` of the checkout at `checkout` track the branch
    /// `merge`, a reference on `origin` such as `refs/heads/main`. The
    /// remote branch need not exist yet.
    ///
    /// # Errors
    ///
    /// Returns the failure of the config command.
    fn set_upstream(&self, checkout: &Path, branch: &str, merge: &str) -> Result<(), Error>;

    /// Points the repository's entry of the worktree at `checkout` at that
    /// directory again, after the directory moved away and back.
    ///
    /// # Errors
    ///
    /// Returns the failure of the repair.
    fn repair_checkout(&self, repository: &Path, checkout: &Path) -> Result<(), Error>;

    /// Creates a worktree of the repository at `checkout` on a detached
    /// `HEAD` at `commit`, and returns the commit.
    ///
    /// # Errors
    ///
    /// Returns the failure of the command that creates the worktree.
    fn add_detached_checkout(
        &self,
        repository: &Path,
        checkout: &Path,
        commit: &str,
    ) -> Result<String, Error>;

    /// Checks out `commit` on a detached `HEAD` in the clone at `checkout`,
    /// writing its files, and returns the commit.
    ///
    /// # Errors
    ///
    /// Returns the failure of the checkout.
    fn detach_head(&self, checkout: &Path, commit: &str) -> Result<String, Error>;
}
