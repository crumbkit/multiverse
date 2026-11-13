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

/// The work of one initialised submodule of a checkout.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SubmoduleWork {
    /// The submodule's path, relative to the checkout.
    pub path: String,
    /// Its uncommitted changes, untracked paths and ignored paths.
    pub changes: Changes,
    /// The commit of each of its stash entries.
    pub stashes: Vec<String>,
    /// Each of its commits that no remote branch holds.
    pub commits: Vec<String>,
}

/// A git operation that can stand paused in a checkout, whose state its git
/// directory holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Operation {
    /// `git merge`, paused on a conflict.
    Merge,
    /// `git rebase`, paused on a conflict or an `edit`.
    Rebase,
    /// `git cherry-pick` of one commit.
    CherryPick,
    /// `git revert` of one commit.
    Revert,
    /// A sequence of cherry-picks or reverts.
    Sequence,
    /// `git bisect`.
    Bisect,
}

/// A git operation that stands paused in a checkout or in one of its
/// submodules.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Paused {
    /// The directory that the operation stands paused in, relative to the
    /// checkout: empty for the checkout itself, or a submodule's path with
    /// `/` after it.
    pub at: String,
    /// The operation.
    pub operation: Operation,
}

/// A file that the index hides from `git status` and whose content differs
/// from the index.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Hidden {
    /// The file, relative to the checkout.
    pub path: String,
    /// Whether the file is marked skip-worktree. Otherwise it is marked
    /// assume-unchanged.
    pub skip_worktree: bool,
}

/// What `git status` does not show about a checkout.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Concealed {
    /// Each repository inside the checkout that git does not track, which
    /// `git add` would store as a commit id without its files.
    pub nested: Vec<String>,
    /// Each file marked skip-worktree or assume-unchanged that differs from
    /// the index.
    pub hidden: Vec<Hidden>,
    /// Each initialised submodule, parents before the submodules inside
    /// them.
    pub submodules: Vec<SubmoduleWork>,
    /// Each file below the checkout that no listing of git accounts for,
    /// such as a file in a submodule directory that is not initialised.
    pub unlisted: Vec<String>,
    /// Each commit of a submodule's git directory that only a reference
    /// outside the branches, tags, notes and remote branches keeps, with
    /// the reference, such as `<commit> (refs/backup/x)`. The checkout's
    /// own such commits are what [`Backend::orphans`] returns, which an
    /// archive keeps.
    pub module_referenced: Vec<String>,
    /// Each git operation that stands paused.
    pub paused: Vec<Paused>,
    /// The stashes and commits of each git directory of a submodule whose
    /// working files are gone, as after `git submodule deinit`, with the
    /// submodule's name as its path.
    pub orphaned: Vec<SubmoduleWork>,
}

/// A stash entry of a clone.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct StashEntry {
    /// The stash's commit.
    pub commit: String,
    /// The message of its reflog entry, such as `WIP on main: 4b825dc Fix`.
    pub message: String,
}

/// The two commits that keep a checkout's uncommitted work, which nothing
/// but the archive's pins names.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct WipCommits {
    /// The commit of the index, whose parent is `HEAD`.
    pub index: String,
    /// The commit of the working files, whose parents are `HEAD` and
    /// `index`.
    pub worktree: String,
}

/// The trees of a checkout's index and of its working files, which a
/// commit of its uncommitted work would hold.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct WipTrees {
    /// The tree of the index.
    pub index: String,
    /// The tree of the working files, without the ignored ones.
    pub worktree: String,
}

/// A file of a checkout's uncommitted work, with its size.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LargeFile {
    /// The file, relative to the checkout.
    pub path: String,
    /// The file's size in bytes.
    pub bytes: u64,
    /// Whether git does not track the file.
    pub untracked: bool,
}

/// The repository that holds a reference, and how git must be told about
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefHolder<'a> {
    /// A bare mirror, which each git command names as its git directory.
    Mirror(&'a Path),
    /// A repository with a work tree, which git discovers from the path.
    Repository(&'a Path),
}

impl<'a> RefHolder<'a> {
    /// Returns the path of the repository.
    #[must_use]
    pub fn path(self) -> &'a Path {
        match self {
            Self::Mirror(path) | Self::Repository(path) => path,
        }
    }
}

/// The names in a clone that its mirror does not hold, which a rebuilt
/// clone does not have.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct LocalNames {
    /// Each linked worktree made from the clone, by its directory, which
    /// stops working when the clone is deleted.
    pub worktrees: Vec<String>,
    /// Each remote other than `origin`.
    pub remotes: Vec<String>,
    /// Each hook in the clone's git directory.
    pub hooks: Vec<String>,
    /// The name of each key of the clone's own configuration that a new
    /// clone does not write. The values may be secrets, so they stay out.
    pub config: Vec<String>,
    /// Each rule of the clone's `info/exclude` that the person's repository
    /// does not hold, which is where a rebuilt clone takes its rules from.
    pub excludes: Vec<String>,
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
    /// Returns [`Error::SshRefused`] when ssh refuses to connect to an SSH
    /// origin, and otherwise the failure of the fetch.
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
    /// Returns [`Error::SshRefused`] when ssh refuses to connect to an SSH
    /// origin, [`Error::MirrorFetchFailed`] when the fetch fails otherwise,
    /// and the failure of setting `HEAD`.
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

    /// Fetches each object of `refs`, a list of a reference and the object
    /// it names, from the clone at `checkout` into the bare `mirror`, and
    /// points each reference of the mirror at its object, durably. The
    /// objects need no reference in the clone.
    ///
    /// # Errors
    ///
    /// Returns the failure of the fetch, and [`Error::CommandFailed`] when a
    /// reference then names another object.
    fn fetch_into_mirror(
        &self,
        mirror: &Path,
        checkout: &Path,
        refs: &[(String, String)],
    ) -> Result<(), Error>;

    /// Returns each reference below `prefix`, such as `refs/heads/`, in the
    /// repository that `holder` names, as its name without the prefix and
    /// the object that it names. An annotated tag names its tag object.
    ///
    /// # Errors
    ///
    /// Returns the failure of the listing.
    fn refs_under(
        &self,
        holder: RefHolder<'_>,
        prefix: &str,
    ) -> Result<Vec<(String, String)>, Error>;

    /// Returns each stash entry of the checkout at `checkout`, newest
    /// first.
    ///
    /// # Errors
    ///
    /// Returns the failure of the listing.
    fn stash_entries(&self, checkout: &Path) -> Result<Vec<StashEntry>, Error>;

    /// Returns the commits of the checkout at `checkout` that `origin` does
    /// not hold and that neither `HEAD` nor any of `kept` reaches, each one
    /// that no other of them reaches. For a clone they come from its
    /// branches, tags, notes and other references, and from the commits
    /// that only the reflog of a detached `HEAD` holds. With `shared`, the
    /// checkout is a worktree, whose branches, tags and other references
    /// stay in its repository, and they come only from the reflog of its
    /// detached `HEAD`.
    ///
    /// # Errors
    ///
    /// Returns the failure of a listing.
    fn orphans(&self, checkout: &Path, kept: &[String], shared: bool)
    -> Result<Vec<String>, Error>;

    /// Returns each file of the uncommitted work of the checkout at
    /// `checkout`, changed, staged or untracked and not ignored, that is
    /// larger than `most` bytes.
    ///
    /// # Errors
    ///
    /// Returns the failure of a listing or of reading a file's size.
    fn large_files(&self, checkout: &Path, most: u64) -> Result<Vec<LargeFile>, Error>;

    /// Returns the trees that a commit of the uncommitted work of the
    /// checkout at `checkout` would hold, written through the temporary
    /// index file `index`, which starts as a copy of the checkout's own.
    /// The checkout's index, branch and references stay as they are. Each
    /// path of `left_out`, relative to the checkout, holds its content in
    /// `HEAD` in both trees. A path that a paused operation left unmerged
    /// holds its content in `HEAD` in the tree of the index and its
    /// working file in the tree of the working files.
    ///
    /// # Errors
    ///
    /// Returns the failure of the copy or of a git command.
    fn wip_trees(
        &self,
        checkout: &Path,
        index: &Path,
        left_out: &[String],
    ) -> Result<WipTrees, Error>;

    /// Writes the two commits of the uncommitted work of the checkout at
    /// `checkout`, as [`Backend::wip_trees`] reads it through `index`
    /// without `left_out`, with `message`, and with author and committer
    /// dates of `time`, seconds since the Unix epoch, so the same work
    /// gives the same commits. The objects are on disk when it returns.
    /// Nothing names the commits until the caller writes a reference.
    ///
    /// # Errors
    ///
    /// Returns the failure of a git command.
    fn write_wip(
        &self,
        checkout: &Path,
        index: &Path,
        left_out: &[String],
        time: u64,
        message: &str,
    ) -> Result<WipCommits, Error>;

    /// Returns the tree of `commit` in the repository at `dir`.
    ///
    /// # Errors
    ///
    /// Returns the failure of the lookup.
    fn tree_of(&self, dir: &Path, commit: &str) -> Result<String, Error>;

    /// Puts the uncommitted work of `wip` back into the clean checkout at
    /// `checkout`: its working files become the tree of `wip.worktree`, and
    /// its index the tree of `wip.index`. Nothing is committed.
    ///
    /// # Errors
    ///
    /// Returns the failure of a git command.
    fn apply_wip(&self, checkout: &Path, wip: &WipCommits) -> Result<(), Error>;

    /// Adds `commit`, a stash commit, to the stash of the checkout at
    /// `checkout` as its newest entry, with `message`.
    ///
    /// # Errors
    ///
    /// Returns the failure of the command.
    fn store_stash(&self, checkout: &Path, commit: &str, message: &str) -> Result<(), Error>;

    /// Creates `reference` at `object` in the repository at `dir`, in one
    /// step that fails when the reference already exists, and reports
    /// whether it created it.
    ///
    /// # Errors
    ///
    /// Returns the failure of the update for another reason.
    fn create_ref(&self, dir: &Path, reference: &str, object: &str) -> Result<bool, Error>;

    /// Returns the path of each submodule that the index of the checkout at
    /// `checkout` names.
    ///
    /// # Errors
    ///
    /// Returns the failure of the listing.
    fn submodule_paths(&self, checkout: &Path) -> Result<Vec<String>, Error>;

    /// Deletes the reference `reference` of the repository that `holder`
    /// names. Deleting a reference that does not exist succeeds.
    ///
    /// # Errors
    ///
    /// Returns the failure of the deletion.
    fn delete_ref(&self, holder: RefHolder<'_>, reference: &str) -> Result<(), Error>;

    /// Returns the commit that `revision` names in the repository that
    /// `holder` names, or `None` when git exits with a failure, which it
    /// does when `revision` names no commit there and when the directory
    /// is not a repository.
    ///
    /// # Errors
    ///
    /// Returns the failure to run git, such as a missing program or a
    /// missed deadline.
    fn resolve_commit(
        &self,
        holder: RefHolder<'_>,
        revision: &str,
    ) -> Result<Option<String>, Error>;

    /// Returns what `git status` does not show about `checkout`.
    ///
    /// # Errors
    ///
    /// Returns the failure of a listing.
    fn concealed(&self, checkout: &Path) -> Result<Concealed, Error>;

    /// Returns what the clone at `checkout` holds that neither an archive
    /// keeps nor a clone rebuilt from its mirror and the person's
    /// repository at `source` has.
    ///
    /// # Errors
    ///
    /// Returns the failure of a listing.
    fn local_names(&self, checkout: &Path, source: &Path) -> Result<LocalNames, Error>;

    /// Deletes the remote `origin` of `checkout`.
    ///
    /// # Errors
    ///
    /// Returns the failure of the command.
    fn remove_origin(&self, checkout: &Path) -> Result<(), Error>;

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

    /// Returns the remote that `branch` of the checkout at `checkout`
    /// tracks a branch of, such as `origin`, or `None` when it tracks none.
    ///
    /// # Errors
    ///
    /// Returns the failure of the lookup.
    fn upstream_remote(&self, checkout: &Path, branch: &str) -> Result<Option<String>, Error>;

    /// Makes `branch` of the checkout at `checkout` track the branch
    /// `merge` of `remote`, a reference on the remote such as
    /// `refs/heads/main`. The remote branch need not exist yet.
    ///
    /// # Errors
    ///
    /// Returns the failure of the config command.
    fn set_upstream(
        &self,
        checkout: &Path,
        branch: &str,
        remote: &str,
        merge: &str,
    ) -> Result<(), Error>;

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
