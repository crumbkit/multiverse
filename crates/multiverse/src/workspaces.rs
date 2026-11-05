//! The workspace operations: create, list, find and remove.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::backend::{Backend, INITIALIZING};
use crate::disposable::{DisposableSet, DisposableSource, PathGlob};
use crate::error::Error;
use crate::git::{FETCH_DEADLINE, LOCAL_DEADLINE};
use crate::lookup::{ManifestSource, checkout_prefix, prefixed};
use crate::manifest::{Manifest, ManifestError};
use crate::mirror::Origin;
use crate::name::WorkspaceName;
use crate::record::{Checkout, Workspace, WorkspaceKind, agents_text};
use crate::store::{self, Created, Records, Store};
use crate::template::Found as FoundTemplate;

#[cfg(test)]
mod clone_tests;
#[cfg(test)]
mod multi_tests;
#[cfg(test)]
mod tests;

/// Whether an operation changes anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The operation makes its changes.
    Change,
    /// The operation reports what it would change, and changes nothing.
    DryRun,
}

/// The workspaces whose records one store keeps and whose checkouts one
/// backend makes.
///
/// A workspace lives in a directory below a root directory that the caller
/// passes to [`Workspaces::create`]. A workspace named `reading-list` of the repository `app` lives at
/// `<root>/reading-list`. That directory holds the workspace's `AGENTS.md`
/// and the checkout `<root>/reading-list/app`, so git never sees the
/// `AGENTS.md`, and a later workspace can hold one checkout for each of
/// several repositories beside it.
#[derive(Debug)]
pub struct Workspaces<B> {
    /// The store of workspace records.
    pub(crate) store: Store,
    /// The backend that makes and removes checkouts.
    pub(crate) backend: B,
    /// The globs of disposable paths that the person declares, which
    /// extend the built-in [`DISPOSABLE`] set and each repository's own
    /// globs in every checkout.
    pub(crate) disposable: Vec<PathGlob>,
    /// The directory of the bare mirrors that the clones borrow objects
    /// from, or `None`, in which case [`Workspaces::create`] makes git
    /// worktrees of the person's repository as mxv did before clones.
    pub(crate) mirrors: Option<PathBuf>,
    /// How long [`Workspaces::create`] waits for the lock of a mirror that
    /// another command fetches, which is [`MIRROR_WAIT`] outside the tests.
    pub(crate) mirror_wait: Duration,
    /// What [`Workspaces::create`] calls, with the mirror and how long it
    /// waits, when another command holds the lock of a mirror, so a person
    /// at a terminal learns why the command pauses.
    pub(crate) mirror_notice: Option<fn(&Path, Duration)>,
}

/// How long `create` waits for another command to finish with a mirror: a
/// whole fetch and a clone without files, which is what the lock covers.
pub const MIRROR_WAIT: Duration = FETCH_DEADLINE.saturating_add(LOCAL_DEADLINE);

/// A repository that a workspace checks out, and the folder that holds its
/// checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repo {
    /// The person's repository, or any directory inside it.
    pub path: PathBuf,
    /// The checkout's folder, which defaults to the repository's name.
    pub folder: Option<String>,
}

impl Repo {
    /// Returns the repository at `path`, checked out in a folder named
    /// after it.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            folder: None,
        }
    }

    /// Returns the repository at `path`, checked out in the folder
    /// `folder`.
    ///
    /// # Errors
    ///
    /// Returns why `folder` is not a folder name, as [`folder_name`]
    /// checks it.
    pub fn named(path: impl Into<PathBuf>, folder: &str) -> Result<Self, String> {
        Ok(Self {
            path: path.into(),
            folder: Some(folder_name(folder)?),
        })
    }
}

/// The names in a workspace's directory that mxv writes itself, which no
/// checkout's folder may take. File systems such as macOS's default ignore
/// case, so they are compared without it.
const RESERVED_FOLDERS: &[&str] = &[crate::record::AGENTS_FILE, crate::manifest::MANIFEST_FILE];

/// Checks `text`, a folder name that the person gives a checkout, and
/// returns it. A folder name follows the rule of a workspace name: 1 to
/// [`crate::MOST_NAME_BYTES`] lowercase ASCII letters, digits, `-`, `_`
/// and `.`, starting with a letter or a digit, without `..`, and not
/// ending with `.` or `.lock`. It may not be `AGENTS.md` or `mxv.toml`.
///
/// # Errors
///
/// Returns why `text` is not a folder name, in a sentence.
pub fn folder_name(text: &str) -> Result<String, String> {
    WorkspaceName::try_from(text.to_owned()).map_err(|error| error.to_string())?;
    if RESERVED_FOLDERS
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(text))
    {
        return Err(format!(
            "`{text}` is a file that mxv writes in the workspace's directory"
        ));
    }
    Ok(text.to_owned())
}

/// Reports whether a checkout's folder named `folder` would take the place
/// of one of `taken`, or of a file that mxv writes in the workspace's
/// directory, on a file system that ignores case.
fn folder_taken<'a>(folder: &str, mut taken: impl Iterator<Item = &'a str>) -> bool {
    RESERVED_FOLDERS
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(folder))
        || taken.any(|other| other.eq_ignore_ascii_case(folder))
}

/// The outcome of [`Workspaces::create`].
#[derive(Debug)]
pub enum NewOutcome {
    /// The workspace was created.
    Created(Workspace),
    /// A dry run found nothing in the way, and the workspace is what a run
    /// would create. Its checkout has no commit, because a dry run does not
    /// fetch.
    Planned(Workspace),
    /// A workspace with the name already exists.
    NameTaken(Workspace),
    /// The workspace's directory already exists, although no record names
    /// it.
    PathTaken(PathBuf),
    /// A checkout's folder in a workspace of several repositories is not a
    /// folder name, as [`AddOutcome::BadFolder`] describes.
    BadFolder {
        /// The folder's name.
        folder: String,
        /// The repository whose checkout would take it.
        repository: PathBuf,
        /// Why the folder is not a folder name, in a sentence.
        reason: String,
    },
    /// The merged manifest that the new workspace of several repositories
    /// would read with the given workspace manifest fails, checked against
    /// the new clones, so the run deleted the workspace.
    ManifestRefused(ManifestError),
    /// The repository already has a local branch with the workspace's name.
    BranchTaken {
        /// The repository's top-level directory.
        repository: PathBuf,
        /// The branch's name.
        branch: String,
    },
    /// Two repositories would have checkouts in the same folder, compared
    /// without case, or a checkout's folder would take the name of a file
    /// that mxv writes in the workspace's directory.
    FolderTaken {
        /// The folder's name.
        folder: String,
        /// The repository whose checkout would take it.
        repository: PathBuf,
    },
}

/// The outcome of [`Workspaces::add`].
#[derive(Debug)]
pub enum AddOutcome {
    /// The checkout was added, and the workspace's record holds it.
    Added(Workspace),
    /// A dry run found nothing in the way, and the workspace is what a run
    /// would make it. The new checkout has no commit.
    Planned(Workspace),
    /// No workspace has the name.
    NotFound,
    /// The workspace's checkouts are git worktrees, which mxv made before
    /// clones, and mxv adds no checkout to such a workspace.
    Worktrees(Workspace),
    /// The new checkout's folder is taken, as [`NewOutcome::FolderTaken`]
    /// describes.
    FolderTaken {
        /// The workspace's record.
        workspace: Workspace,
        /// The folder's name.
        folder: String,
        /// The repository whose checkout would take it.
        repository: PathBuf,
    },
    /// The repository already has a local branch with the workspace's name.
    BranchTaken {
        /// The repository's top-level directory.
        repository: PathBuf,
        /// The branch's name.
        branch: String,
    },
    /// The new checkout's folder is not a folder name, as [`folder_name`]
    /// describes. A folder that the repository's name gives is checked as
    /// one that `--as` gives, because its prefix names units.
    BadFolder {
        /// The folder's name.
        folder: String,
        /// The repository whose checkout would take it.
        repository: PathBuf,
        /// Why the folder is not a folder name, in a sentence.
        reason: String,
    },
    /// The workspace has one checkout and a workspace manifest, and the
    /// caller gave no manifest. That manifest's paths are relative to the
    /// checkout, and once the workspace has two checkouts they would be
    /// relative to the workspace's directory, so the caller must give the
    /// workspace manifest that the workspace reads afterwards.
    ManifestNeeded {
        /// The workspace's record.
        workspace: Workspace,
        /// The workspace manifest.
        manifest: PathBuf,
        /// The folder of the workspace's one checkout.
        folder: String,
    },
    /// The merged manifest that the workspace would read with the new
    /// checkout fails, so the run deleted the new clone and changed
    /// nothing else.
    ManifestRefused(ManifestError),
    /// An earlier `add` stopped before it finished the checkout in
    /// `folder`, and this run adds another checkout. Only the same `add`
    /// resumes it.
    Incomplete {
        /// The workspace's record.
        workspace: Workspace,
        /// The folder of the unfinished checkout.
        folder: String,
        /// The repository of the unfinished checkout.
        repository: PathBuf,
    },
    /// The run resumes an unfinished checkout whose directory holds
    /// something that mxv does not delete, such as commits that origin
    /// lacks or files outside a clone.
    ResumeBlocked {
        /// The workspace's record.
        workspace: Workspace,
        /// The directory of the unfinished checkout.
        path: PathBuf,
        /// What the directory holds, in a sentence.
        reason: String,
    },
}

/// What the directory of an unfinished checkout holds when `add` resumes
/// it.
#[derive(Debug, PartialEq, Eq)]
enum Leftover {
    /// Nothing is at the path, so the run clones afresh.
    Absent,
    /// A clone that stopped before it reached the workspace's branch, and
    /// that holds no commit or stash of its own, or a directory that holds
    /// nothing but `.git`. The run deletes it and clones afresh.
    Debris,
    /// A clone of its own on the workspace's branch, at this commit. Only
    /// the record's commit and `AGENTS.md` were left, so the run keeps it.
    Finished(String),
    /// Something that mxv does not delete, with the reason.
    Kept(String),
}

/// What removing a workspace does, or did, to one of its checkouts.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckoutRemoval {
    /// The checkout.
    pub checkout: Checkout,
    /// What happens to the checkout's directory.
    pub directory: Fate,
    /// What happens to the checkout's branch.
    pub branch: BranchFate,
    /// The commit that the checkout's branch pointed at when the removal
    /// was planned, or `None` when the branch was gone. The removal deletes
    /// the branch only while it still points at this commit.
    pub tip: Option<String>,
    /// The number of commits that the checkout's `HEAD` holds and no
    /// branch holds, such as commits on a detached `HEAD`, which the
    /// removal loses. For a clone, it is the number of commits that
    /// `origin` does not hold, which `lost` lists.
    pub stray_commits: u32,
    /// The commits of a clone that `origin` does not hold, which the
    /// removal loses, or none for a worktree.
    pub lost: Vec<String>,
    /// The commit of each stash entry of a clone, which the removal
    /// deletes, or none for a worktree.
    pub stashes: Vec<String>,
    /// Whether the removal unlocks the checkout first, because a killed
    /// `git worktree add` left it locked with the reason
    /// [`INITIALIZING`].
    pub unlock: bool,
    /// The checkout's disposable directories, relative to the checkout,
    /// such as `target/`. The removal deletes them by file descriptor
    /// before it hands the checkout to git or deletes the clone, so no
    /// deadline of git's covers the bulk of the deletion.
    pub disposable: Vec<String>,
    /// Whether the checkout is a worktree whose `.git` file is gone,
    /// because an earlier removal stopped partway. Git can no longer read
    /// it, so the removal deletes what is left by file descriptor and then
    /// prunes the repository's entry, and only when the caller allowed
    /// discarding changes.
    pub partial: bool,
}

/// What removing a workspace does to a checkout's directory or to the
/// workspace's directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    /// The directory is removed.
    Removed,
    /// The directory was already gone.
    Missing,
    /// The directory stays, because it still holds files that mxv did not
    /// write.
    Kept,
}

/// What removing a workspace does to a checkout's branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BranchFate {
    /// The branch is deleted, because every commit on it is on its base.
    Deleted,
    /// The branch stays, because it holds commits that are not on its base
    /// and the caller did not allow deleting them.
    KeptUnmerged {
        /// The number of commits on the branch that are not on its base, or
        /// `None` when its base names no commit any more, so the number is
        /// unknown.
        commits: Option<u32>,
    },
    /// The branch is deleted with commits that are not on its base,
    /// because the caller allowed it.
    DeletedUnmerged {
        /// The number of commits on the branch that are not on its base, or
        /// `None` when its base names no commit any more.
        commits: Option<u32>,
    },
    /// The branch stays, because it pointed at another commit when the
    /// removal came to delete it than when the removal was planned, so it
    /// may hold work that the plan did not show.
    KeptMoved,
    /// The branch, or its repository, was already gone.
    Missing,
}

/// What the caller allows a removal to lose. Each permission is separate,
/// so discarding changes never deletes commits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Allow {
    /// Allows discarding uncommitted changes, untracked files and ignored
    /// files outside [`DISPOSABLE`], as `mxv remove --force` does.
    pub discard_changes: bool,
    /// Allows deleting commits that are on no other branch: an unmerged
    /// branch, and commits on a detached `HEAD`, as
    /// `mxv remove --delete-unmerged` does.
    pub delete_unmerged: bool,
}

/// The names of ignored directories that a removal deletes without asking,
/// because a build or an install makes them again. An ignored path is
/// disposable when git reports it as a whole ignored directory, with a
/// trailing `/`, whose own name is one of these, such as `target/` or
/// `web/node_modules/`, or when a glob of the repository's manifest or of
/// [`Workspaces::with_disposable`] matches it. Every other ignored path
/// stops a removal unless the caller allows discarding changes, including
/// `.env`, an ignored file named `build`, an ignored file inside a
/// tracked `dist/` directory, such as `tools/dist/prod.secret`, and a
/// disposable directory that holds a `.git` entry at any depth.
pub const DISPOSABLE: &[&str] = &["target", "node_modules", "dist", "build", ".cache"];

/// Reports whether `fresh`, a new plan of the removal that the caller
/// confirmed as `confirmed`, would lose more than `confirmed` showed: a
/// different record, more commits that no branch holds in a checkout, a
/// clone's commit or stash that `confirmed` did not list, or a change or
/// ignored path to discard that `confirmed` did not list.
fn loses_more(fresh: &Removal, confirmed: &Removal) -> bool {
    fresh.workspace != confirmed.workspace
        || fresh
            .checkouts
            .iter()
            .zip(&confirmed.checkouts)
            .any(|(now, seen)| {
                now.stray_commits > seen.stray_commits
                    || now.lost.iter().any(|commit| !seen.lost.contains(commit))
                    || now
                        .stashes
                        .iter()
                        .any(|stash| !seen.stashes.contains(stash))
            })
        || fresh
            .discarded
            .iter()
            .any(|path| !confirmed.discarded.contains(path))
        || fresh
            .manifest
            .iter()
            .any(|path| !confirmed.manifest.contains(path))
}

/// What removing a workspace does, or did.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Removal {
    /// The workspace's record, which is deleted.
    pub workspace: Workspace,
    /// What happens to each checkout, in the record's order.
    pub checkouts: Vec<CheckoutRemoval>,
    /// What happens to the workspace's directory.
    pub directory: Fate,
    /// What the caller allowed the removal to lose.
    pub allow: Allow,
    /// The ignored paths in [`DISPOSABLE`] that the removal deletes, with
    /// the checkout's name in front, such as `app: target/`.
    pub disposable: Vec<String>,
    /// The changes and other ignored paths that the removal discards
    /// because the caller allowed it, in the form of
    /// [`RemovalPlan::Dirty`] and [`RemovalPlan::IgnoredFiles`].
    pub discarded: Vec<String>,
    /// The paths of `disposable` that only a glob of the repository's own
    /// `mxv.toml` makes disposable, which a caller deletes only after the
    /// person confirms them or allows discarding changes.
    pub manifest: Vec<String>,
    /// Why a checkout's disposable set holds fewer globs than its
    /// `mxv.toml` declares, such as an invalid manifest, or why its paths
    /// wait for a confirmation.
    pub warnings: Vec<String>,
    /// The workspace manifest, [`Workspace::manifest_file`], which the
    /// removal deletes, or `None` when the workspace has none. A person may
    /// have edited it by hand, so the removal names it.
    pub workspace_manifest: Option<PathBuf>,
}

/// The outcome of [`Workspaces::remove`].
#[derive(Debug)]
pub enum RemovalOutcome {
    /// The removal ran, and holds what it did.
    Done(Removal),
    /// The workspace changed after the plan, so a new plan refuses the
    /// removal or would lose more than the plan showed, and nothing was
    /// removed. It holds the new plan. A new plan that is
    /// [`RemovalPlan::Ready`] would lose more commits that no branch holds,
    /// or discard more changes, than the plan that the caller confirmed.
    Changed(RemovalPlan),
}

/// The outcome of [`Workspaces::plan_removal`].
#[derive(Debug)]
pub enum RemovalPlan {
    /// Nothing is in the way, and [`Workspaces::remove`] carries out the
    /// removal.
    Ready(Removal),
    /// No workspace has the name.
    NotFound,
    /// A checkout has uncommitted or untracked changes, and the caller did
    /// not force the removal.
    Dirty {
        /// The workspace's record.
        workspace: Workspace,
        /// Each change, as `git status --porcelain` prints it, with the
        /// checkout's directory name in front, such as `app: ?? notes.md`.
        changes: Vec<String>,
    },
    /// A checkout holds ignored files outside [`DISPOSABLE`], such as
    /// `.env`, and the caller did not allow discarding changes.
    IgnoredFiles {
        /// The workspace's record.
        workspace: Workspace,
        /// Each ignored path, with the checkout's name in front, such as
        /// `app: .env`.
        paths: Vec<String>,
        /// Why a checkout's disposable set holds fewer globs than its
        /// `mxv.toml` declares, such as an invalid manifest.
        warnings: Vec<String>,
    },
    /// A checkout's `HEAD` holds commits that no branch holds, such as
    /// commits on a detached `HEAD`, and the caller did not allow deleting
    /// unmerged commits.
    StrayCommits {
        /// The workspace's record.
        workspace: Workspace,
        /// The checkout's directory.
        checkout: PathBuf,
        /// The number of commits that removing the checkout would lose.
        commits: u32,
    },
    /// A clone holds commits that no remote branch of its `origin` holds,
    /// after a fetch, and the caller did not allow deleting unmerged
    /// commits. Deleting the clone would lose them.
    Unpushed {
        /// The workspace's record.
        workspace: Workspace,
        /// The clone's directory.
        checkout: PathBuf,
        /// The number of commits that deleting the clone would lose.
        commits: u32,
    },
    /// A clone holds stash entries, and the caller did not allow
    /// discarding changes. Deleting the clone would lose them.
    Stashed {
        /// The workspace's record.
        workspace: Workspace,
        /// The clone's directory.
        checkout: PathBuf,
        /// The number of stash entries.
        stashes: u32,
    },
    /// A checkout is locked for a reason other than
    /// [`INITIALIZING`], so something else may
    /// still use it, such as a checkout on a removable disk.
    Locked {
        /// The workspace's record.
        workspace: Workspace,
        /// The checkout's directory.
        checkout: PathBuf,
        /// The lock's reason, which is empty when the lock gives none.
        reason: String,
    },
    /// The workspace's directory or a checkout's directory is a symbolic
    /// link, or a checkout's directory exists but its repository does not
    /// list it as a checkout, so the directory is not the workspace's to
    /// remove (DUR-4).
    Unowned {
        /// The workspace's record.
        workspace: Workspace,
        /// The directory that the record names.
        checkout: PathBuf,
    },
}

/// The changes and ignored paths of a workspace's checkouts, sorted by what
/// a removal does with them.
#[derive(Debug, Default)]
struct Sorted {
    /// The uncommitted changes and untracked paths.
    changes: Vec<String>,
    /// The ignored paths outside the disposable set.
    ignored: Vec<String>,
    /// The disposable paths.
    disposables: Vec<String>,
    /// Why a checkout's disposable set holds fewer globs than its manifest
    /// declares.
    warnings: Vec<String>,
    /// The disposable paths that only the manifest makes disposable.
    manifest: Vec<String>,
}

/// Why a clone cannot be removed without the caller's leave.
#[derive(Debug)]
enum CloneRefusal {
    /// The directory is not the workspace's clone.
    Unowned(PathBuf),
    /// The clone holds commits that `origin` does not hold.
    Unpushed {
        /// The clone's directory.
        checkout: PathBuf,
        /// The number of such commits.
        commits: u32,
    },
    /// The clone holds stash entries.
    Stashed {
        /// The clone's directory.
        checkout: PathBuf,
        /// The number of stash entries.
        stashes: u32,
    },
}

impl CloneRefusal {
    /// Returns the plan that refuses the removal of `workspace`.
    fn plan(self, workspace: Workspace) -> RemovalPlan {
        match self {
            Self::Unowned(checkout) => RemovalPlan::Unowned {
                workspace,
                checkout,
            },
            Self::Unpushed { checkout, commits } => RemovalPlan::Unpushed {
                workspace,
                checkout,
                commits,
            },
            Self::Stashed { checkout, stashes } => RemovalPlan::Stashed {
                workspace,
                checkout,
                stashes,
            },
        }
    }
}

/// Reports whether the worktree `checkout`, whose directory exists, has
/// lost its `.git` file, as a `git worktree remove` that stopped partway
/// leaves it. Git cannot read such a checkout, so mxv cannot tell what is
/// left in it.
fn is_partial(checkout: &Checkout) -> Result<bool, Error> {
    Ok(Entry::at(&checkout.path.join(".git"))? == Entry::Absent)
}

/// Returns the refusal of a removal of `workspace` without leave to
/// discard changes, whose worktree named `checkout` [`is_partial`].
fn partial_refusal(workspace: Workspace, checkout: &str) -> RemovalPlan {
    RemovalPlan::Dirty {
        changes: vec![format!(
            "{checkout}: the checkout has no .git, because an earlier removal stopped partway, so mxv cannot tell what is left in it"
        )],
        workspace,
    }
}

/// The suffix of the name under which mxv moves a clone aside before it
/// deletes it, such as `.app.mxv-removing` beside `app`.
const ASIDE_SUFFIX: &str = ".mxv-removing";

/// Returns the path beside `checkout` under which [`delete_clone`] moves
/// it before deleting it, or `None` for a path without a name.
fn aside(checkout: &Path) -> Option<PathBuf> {
    let name = checkout.file_name()?.to_str()?;
    Some(checkout.with_file_name(format!(".{name}{ASIDE_SUFFIX}")))
}

/// Deletes the clone at `checkout` and everything in it. It first moves
/// the clone aside, to the name that [`aside`] returns, in one rename, so
/// a deletion that stops partway leaves no checkout behind that has lost
/// its `.git`, and the next removal finds the clone gone and deletes what
/// is left aside. It then deletes the moved clone as [`delete_directory`]
/// does. A clone that is already gone is no error.
fn delete_clone(checkout: &Path) -> Result<(), Error> {
    let Some(aside) = aside(checkout) else {
        return Err(Error::Unwritable {
            path: checkout.to_path_buf(),
            source: io::ErrorKind::InvalidInput.into(),
        });
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "moving a clone aside to delete it writes no durable file, so it does not go through the store module (DUR-1)"
    )]
    let moved = fs::rename(checkout, &aside);
    match moved {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(Error::Unwritable {
                path: checkout.to_path_buf(),
                source,
            });
        }
    }
    delete_directory(&aside)
}

/// Deletes the directory `path` and everything in it by file descriptor,
/// as `mxv clean` deletes a disposable path, so a directory that another
/// process swaps for a symbolic link is never followed and nothing outside
/// `path` is touched. No deadline covers it. A symbolic link at `path` is
/// deleted itself, and a directory that is already gone is no error.
fn delete_directory(path: &Path) -> Result<(), Error> {
    let unwritable = |source| Error::Unwritable {
        path: path.to_path_buf(),
        source,
    };
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(unwritable(source)),
    }
    let (Some(parent), Some(name)) = (
        path.parent(),
        path.file_name().and_then(|name| name.to_str()),
    ) else {
        return Err(unwritable(io::ErrorKind::InvalidInput.into()));
    };
    crate::disk::remove_tree(parent, &format!("{name}/"))
        .map_err(|(path, source)| Error::Unwritable { path, source })
}

/// Whether something is at `path`, without following a symbolic link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    /// Nothing is at the path.
    Absent,
    /// A symbolic link is at the path.
    Link,
    /// A file or a directory is at the path.
    Present,
}

impl Entry {
    /// Returns what is at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Inaccessible`] when mxv cannot tell, such as when a
    /// parent directory cannot be searched, so nothing treats the path as
    /// present or absent by guess.
    pub(crate) fn at(path: &Path) -> Result<Self, Error> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => Ok(Self::Link),
            Ok(_) => Ok(Self::Present),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::Absent),
            Err(source) => Err(Error::Inaccessible {
                path: path.to_path_buf(),
                source,
            }),
        }
    }
}

/// What a run of [`Workspaces::make`] created, so that
/// [`Workspaces::undo`] deletes that and nothing else.
#[derive(Debug, Default)]
struct Made {
    /// Whether the run created the workspace's directory.
    directory: bool,
    /// The commit at which the run created each checkout's branch, in the
    /// record's order, for the checkouts whose branch it created. A
    /// checkout whose `worktree add` then failed counts, because git may
    /// have left its directory.
    branches: Vec<String>,
    /// Whether the run wrote `AGENTS.md`.
    agents: bool,
    /// Whether the run wrote the workspace manifest.
    manifest: bool,
    /// The number of checkouts, in the record's order, whose clone the run
    /// started, which may have left a directory behind.
    clones: usize,
}

/// How [`Workspaces::make`] ended without an error.
#[derive(Debug, PartialEq, Eq)]
enum Making {
    /// The workspace is complete.
    Done,
    /// Another process created a checkout's branch after
    /// [`Workspaces::create`] checked for it, so the run left that branch
    /// alone and stopped.
    BranchTaken {
        /// The checkout's repository.
        repository: PathBuf,
        /// The branch.
        branch: String,
    },
}

impl<B: Backend> Workspaces<B> {
    /// Returns the workspaces with their records in `store` and their
    /// checkouts made by `backend`.
    #[must_use]
    pub const fn new(store: Store, backend: B) -> Self {
        Self {
            store,
            backend,
            disposable: Vec::new(),
            mirrors: None,
            mirror_wait: MIRROR_WAIT,
            mirror_notice: None,
        }
    }

    /// Returns the workspaces with `dir` as the directory of the bare
    /// mirrors, so [`Workspaces::create`] makes each checkout a clone of a
    /// mirror there, with a `.git` of its own.
    #[must_use]
    pub fn with_mirrors(mut self, dir: PathBuf) -> Self {
        self.mirrors = Some(dir);
        self
    }

    /// Returns the workspaces with `notice`, which [`Workspaces::create`]
    /// calls once, with the mirror's directory and how long it will wait,
    /// before it waits for another command to release the lock of a
    /// mirror.
    #[must_use]
    pub fn with_mirror_notice(mut self, notice: fn(&Path, Duration)) -> Self {
        self.mirror_notice = Some(notice);
        self
    }

    /// Returns the workspaces with `globs`, the globs of disposable paths
    /// that the person declares, added to the built-in [`DISPOSABLE`] set
    /// and to each repository's own globs.
    #[must_use]
    pub fn with_disposable(mut self, globs: Vec<PathGlob>) -> Self {
        self.disposable = globs;
        self
    }

    /// Returns the disposable set of `checkout` of `workspace`: the
    /// built-in [`DISPOSABLE`] directories, the globs of the one manifest
    /// that the checkout reads, and the person's globs. The manifest is
    /// the workspace manifest when it exists, whose globs count as the
    /// person's own ([`DisposableSource::Workspace`]), or else the
    /// checkout's `mxv.toml`, whose globs only it declares
    /// ([`DisposableSource::Manifest`]). A manifest that cannot be read
    /// adds no globs, so fewer paths are disposable, and the returned
    /// warning says so.
    pub(crate) fn disposable_set(
        &self,
        workspace: &Workspace,
        checkout: &Checkout,
    ) -> (DisposableSet, Option<String>) {
        if workspace.has_several_checkouts() {
            return self.merged_disposable_set(workspace, checkout);
        }
        let inputs = workspace.manifest_inputs_for(Some(checkout));
        // Only a workspace manifest that no repository can have written
        // counts like the config file. One that is a link counts like the
        // repository's own, whose paths wait for a confirmation.
        let linked = inputs.source() == ManifestSource::Workspace && !workspace.manifest_trusted();
        let source = if inputs.source() == ManifestSource::Workspace && !linked {
            DisposableSource::Workspace
        } else {
            DisposableSource::Manifest
        };
        let (declared, warning) = match workspace.manifest_for(Some(checkout)) {
            Ok(manifest) => (
                manifest
                    .map(|manifest| manifest.disposable)
                    .unwrap_or_default(),
                linked.then(|| {
                    format!(
                        "the workspace manifest {} is a symbolic link or a hard link of another file, so mxv deletes the paths it declares disposable only after a confirmation, as it does for a repository's mxv.toml",
                        workspace.manifest_file().display()
                    )
                }),
            ),
            Err(error) => (
                Vec::new(),
                Some(format!(
                    "the manifest {} of the checkout {} is invalid, so mxv uses only the built-in disposable set and the config file's globs there: {error}",
                    error.path.display(),
                    checkout.name
                )),
            ),
        };
        (
            DisposableSet::built_in()
                .with(&declared, source)
                .with(&self.disposable, DisposableSource::Config),
            warning,
        )
    }

    /// Returns the disposable set of `checkout` of `workspace`, a
    /// workspace with several checkouts: the built-in [`DISPOSABLE`]
    /// directories, the globs of the checkout's own `mxv.toml`
    /// ([`DisposableSource::Manifest`]), the globs of the workspace
    /// manifest that apply in the checkout, as [`PathGlob::within`]
    /// decides, and the person's globs. The workspace manifest's globs
    /// count as the person's own ([`DisposableSource::Workspace`]) only
    /// while [`Workspace::manifest_trusted`] holds. A manifest that cannot
    /// be read adds no globs, and the returned warning says so.
    fn merged_disposable_set(
        &self,
        workspace: &Workspace,
        checkout: &Checkout,
    ) -> (DisposableSet, Option<String>) {
        let mut set = DisposableSet::built_in();
        let mut warnings = Vec::new();
        match crate::manifest::Manifest::read(&checkout.path) {
            Ok(manifest) => {
                if let Some(manifest) = manifest {
                    set = set.with(&manifest.disposable, DisposableSource::Manifest);
                }
            }
            Err(error) => warnings.push(format!(
                "the manifest {} of the checkout {} is invalid, so mxv uses only the built-in disposable set, the workspace manifest's globs and the config file's globs there: {error}",
                error.path.display(),
                checkout.name
            )),
        }
        match workspace.workspace_manifest() {
            Ok(Some(manifest)) => {
                let globs: Vec<PathGlob> = manifest
                    .disposable
                    .iter()
                    .filter_map(|glob| glob.within(&checkout.name))
                    .collect();
                let source = if workspace.manifest_trusted() {
                    DisposableSource::Workspace
                } else {
                    warnings.push(format!(
                        "the workspace manifest {} is a symbolic link or a hard link of another file, so mxv deletes the paths it declares disposable only after a confirmation, as it does for a repository's mxv.toml",
                        workspace.manifest_file().display()
                    ));
                    DisposableSource::Manifest
                };
                set = set.with(&globs, source);
            }
            Ok(None) => {}
            Err(error) => warnings.push(format!(
                "the workspace manifest {} is invalid, so mxv adds none of its globs in the checkout {}: {error}",
                error.path.display(),
                checkout.name
            )),
        }
        let warning = (!warnings.is_empty()).then(|| warnings.join("; "));
        (
            set.with(&self.disposable, DisposableSource::Config),
            warning,
        )
    }

    /// Returns every workspace, in name order, and a problem for each
    /// record that cannot be read.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Store::list`].
    pub fn list(&self) -> Result<Records, Error> {
        self.store.list()
    }

    /// Returns the workspace named `name`, or `None`.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Store::read`].
    pub fn find(&self, name: &WorkspaceName) -> Result<Option<Workspace>, Error> {
        self.store.read(name)
    }

    /// Creates the workspace `name` in the directory `<root>/<name>`, with
    /// one checkout of the repository at `repository`, on a new branch
    /// `name` from the freshly fetched default branch of `origin`.
    ///
    /// With [`Workspaces::with_mirrors`], the checkout is a clone. `create`
    /// takes the lock of the origin's mirror, makes the mirror when it does
    /// not exist, fetches it from the origin, and clones it with
    /// `--shared`, all under the lock, and then sets the clone's `origin`
    /// to the origin itself and checks out the branch. The default branch
    /// is the one that the person's repository names as `origin/HEAD`, and
    /// the person's repository is only read. Without mirrors, the checkout
    /// is a git worktree of the person's repository, as mxv made them
    /// before clones.
    ///
    /// It writes the record before it makes the checkout, so a run that
    /// stops partway leaves a record that [`Workspaces::remove`] can clean
    /// up, and a failed checkout deletes the record and the directory
    /// again. A dry run reads the default branch that the repository last
    /// fetched, and fetches nothing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RepositoryNotFound`] or [`Error::NoOrigin`] for a
    /// path that is not a repository with an `origin`, the failures of the
    /// fetch and the checkout, [`Error::MirrorBusy`] when another command
    /// holds the mirror's lock for longer than [`MIRROR_WAIT`],
    /// [`Error::MirrorFetchFailed`] when the mirror's fetch fails, and
    /// [`Error::Unwritable`] when a directory, the record or `AGENTS.md`
    /// cannot be written.
    pub fn create(
        &self,
        root: &Path,
        name: &WorkspaceName,
        repository: &Path,
        mode: Mode,
    ) -> Result<NewOutcome, Error> {
        self.create_with_manifest(root, name, repository, mode, None)
    }

    /// Returns the template in `templates` for the origin of the
    /// repository at `repository`, as [`find_template`] describes, and the
    /// templates it skipped. `root` is the root of the workspaces, under
    /// which no template may lie.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RepositoryNotFound`] or [`Error::NoOrigin`] for a
    /// path that is not a repository with an `origin`, and the errors of
    /// [`find_template`].
    ///
    /// [`find_template`]: crate::find_template
    pub fn template(
        &self,
        templates: &Path,
        repository: &Path,
        root: &Path,
    ) -> Result<FoundTemplate, Error> {
        let repository = self.backend.repository(repository)?;
        let url = self.backend.origin_url(&repository.root)?;
        crate::template::find(templates, &url, &repository.root, root)
    }

    /// Returns the top-level directory of the repository of `repo` and the
    /// folder that its checkout gets: the one that `repo` gives, or else
    /// the repository's name.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RepositoryNotFound`] for a path that is not in a
    /// git repository.
    pub fn resolve(&self, repo: &Repo) -> Result<(PathBuf, String), Error> {
        let repository = self.backend.repository(&repo.path)?;
        let folder = repo.folder.clone().unwrap_or(repository.name);
        Ok((repository.root, folder))
    }

    /// Checks that `path`, a file that the person names to copy as a
    /// workspace manifest, is the person's own. A workspace manifest's
    /// `disposable` globs are deleted without asking, so mxv refuses a file
    /// that a repository or an agent could have written: one that lies,
    /// once symbolic links are resolved, under `root`, the root of the
    /// workspaces, or inside a git work tree.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ManifestUntrusted`] for such a file, and the errors
    /// of git other than [`Error::RepositoryNotFound`] and
    /// [`Error::NoOrigin`].
    pub fn check_manifest_source(&self, path: &Path, root: &Path) -> Result<(), Error> {
        let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let untrusted = |reason: String| Error::ManifestUntrusted {
            path: path.to_path_buf(),
            reason,
        };
        if resolved.starts_with(&root) {
            return Err(untrusted(format!(
                "it lies under the root of the workspaces {}",
                root.display()
            )));
        }
        let Some(dir) = resolved.parent() else {
            return Ok(());
        };
        match self.backend.repository(dir) {
            Ok(repository) => Err(untrusted(format!(
                "it lies inside the git work tree {}",
                repository.root.display()
            ))),
            Err(Error::NoOrigin { repository }) => Err(untrusted(format!(
                "it lies inside the git work tree {}",
                repository.display()
            ))),
            Err(Error::RepositoryNotFound { .. }) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Creates the workspace as [`Workspaces::create`] does, and writes
    /// `manifest`, when it is given, as the workspace manifest
    /// [`Workspace::manifest_file`], beside `AGENTS.md`. A failure after
    /// the manifest was written deletes it with the rest of the workspace.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Workspaces::create`], and
    /// [`Error::Unwritable`] when the manifest cannot be written.
    pub fn create_with_manifest(
        &self,
        root: &Path,
        name: &WorkspaceName,
        repository: &Path,
        mode: Mode,
        manifest: Option<&str>,
    ) -> Result<NewOutcome, Error> {
        self.create_repos(root, name, &[Repo::new(repository)], mode, manifest)
    }

    /// Creates the workspace `name` as [`Workspaces::create_with_manifest`]
    /// does, with one checkout of each of `repos`, in the order given. Each
    /// checkout lives in `<root>/<name>/<folder>`, where the folder is the
    /// repository's name or the one that [`Repo::folder`] gives, and each
    /// is on a new branch `name`. A failure of any checkout deletes every
    /// checkout that the run made.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Workspaces::create`].
    pub fn create_repos(
        &self,
        root: &Path,
        name: &WorkspaceName,
        repos: &[Repo],
        mode: Mode,
        manifest: Option<&str>,
    ) -> Result<NewOutcome, Error> {
        if let Some(existing) = self.store.read(name)? {
            return Ok(NewOutcome::NameTaken(existing));
        }
        let path = root.join(name.as_str());
        let mut found: Vec<(crate::backend::Repository, String)> = Vec::new();
        for repo in repos {
            let repository = self.backend.repository(&repo.path)?;
            let folder = repo
                .folder
                .clone()
                .unwrap_or_else(|| repository.name.clone());
            // A workspace of one repository gives its checkout's names no
            // prefix, so any repository's name works there.
            if repos.len() > 1
                && let Err(reason) = folder_name(&folder)
            {
                return Ok(NewOutcome::BadFolder {
                    folder,
                    repository: repository.root,
                    reason,
                });
            }
            if folder_taken(&folder, found.iter().map(|(_, taken)| taken.as_str())) {
                return Ok(NewOutcome::FolderTaken {
                    folder,
                    repository: repository.root,
                });
            }
            found.push((repository, folder));
        }
        if Entry::at(&path)? != Entry::Absent {
            return Ok(NewOutcome::PathTaken(path));
        }
        // A clone's branch lives in the clone, and a branch of the same
        // name in the person's repository would still be confusing once
        // either is pushed, so both kinds refuse it.
        for (repository, _) in &found {
            if self
                .backend
                .branch_tip(&repository.root, name.as_str())?
                .is_some()
            {
                return Ok(NewOutcome::BranchTaken {
                    repository: repository.root.clone(),
                    branch: name.to_string(),
                });
            }
        }
        if let Some(mirrors) = &self.mirrors {
            return self.create_clone(root, name, &found, mirrors, mode, manifest);
        }
        let mut checkouts = Vec::new();
        for (repository, folder) in &found {
            if mode == Mode::Change {
                self.backend.fetch(&repository.root)?;
            }
            let base = self.backend.base(&repository.root)?;
            checkouts.push(Checkout::new(
                folder.clone(),
                &repository.root,
                path.join(folder),
                name.to_string(),
                base,
            ));
        }
        let mut workspace = Workspace::new(name.clone(), path, checkouts).of_worktrees();
        if mode == Mode::DryRun {
            return Ok(NewOutcome::Planned(workspace));
        }
        if self.store.create(&mut workspace)? == Created::Taken {
            return Ok(self.store.read(name)?.map_or(
                NewOutcome::PathTaken(workspace.path.clone()),
                NewOutcome::NameTaken,
            ));
        }
        let mut made = Made::default();
        match self.make(root, &mut workspace, manifest, &mut made) {
            Ok(Making::Done) => Ok(NewOutcome::Created(workspace)),
            Ok(Making::BranchTaken { repository, branch }) => {
                self.undo(&workspace, &made);
                Ok(NewOutcome::BranchTaken { repository, branch })
            }
            Err(error) => {
                self.undo(&workspace, &made);
                Err(error)
            }
        }
    }

    /// Returns the checkout of `repository` in the folder `folder` of the
    /// workspace `name` at `path`, as a clone of the mirror of its origin
    /// below `mirrors`, and the origin's URL.
    fn planned_clone(
        &self,
        name: &WorkspaceName,
        path: &Path,
        repository: &crate::backend::Repository,
        folder: &str,
        mirrors: &Path,
    ) -> Result<(Checkout, String), Error> {
        let url = self.backend.origin_url(&repository.root)?;
        let origin = Origin::parse(&url, &repository.root).ok_or_else(|| Error::NoOrigin {
            repository: repository.root.clone(),
        })?;
        let base = self.backend.base(&repository.root)?;
        let mut checkout = Checkout::new(
            folder.to_owned(),
            &repository.root,
            path.join(folder),
            name.to_string(),
            base,
        );
        checkout.mirror = Some(mirrors.join(&origin.key));
        Ok((checkout, origin.url))
    }

    /// Creates the workspace `name` of [`Workspaces::create_repos`] with a
    /// clone of the mirror below `mirrors` of each repository's origin.
    fn create_clone(
        &self,
        root: &Path,
        name: &WorkspaceName,
        found: &[(crate::backend::Repository, String)],
        mirrors: &Path,
        mode: Mode,
        manifest: Option<&str>,
    ) -> Result<NewOutcome, Error> {
        let path = root.join(name.as_str());
        let mut checkouts = Vec::new();
        let mut urls = Vec::new();
        for (repository, folder) in found {
            let (checkout, url) = self.planned_clone(name, &path, repository, folder, mirrors)?;
            checkouts.push(checkout);
            urls.push(url);
        }
        let mut workspace = Workspace::new(name.clone(), path, checkouts);
        if mode == Mode::DryRun {
            return Ok(NewOutcome::Planned(workspace));
        }
        if self.store.create(&mut workspace)? == Created::Taken {
            return Ok(self.store.read(name)?.map_or(
                NewOutcome::PathTaken(workspace.path.clone()),
                NewOutcome::NameTaken,
            ));
        }
        let mut made = Made::default();
        match self.make_clones(root, &mut workspace, &urls, manifest, &mut made) {
            Ok(Ok(())) => Ok(NewOutcome::Created(workspace)),
            Ok(Err(refused)) => {
                self.undo(&workspace, &made);
                Ok(NewOutcome::ManifestRefused(refused))
            }
            Err(error) => {
                self.undo(&workspace, &made);
                Err(error)
            }
        }
    }

    /// Adds a checkout of `repo` to the workspace `name`, as a clone of the
    /// mirror of its origin on the branch `name`, in the folder
    /// `<workspace>/<folder>`, and writes `AGENTS.md` and the record again.
    ///
    /// `manifest`, when it is given, is the text of the workspace manifest
    /// that the workspace reads once it has the new checkout, with its
    /// paths relative to the workspace's directory. A workspace that has
    /// one checkout and a workspace manifest needs it, because that
    /// manifest's paths are relative to the checkout, and `add` returns
    /// [`AddOutcome::ManifestNeeded`] without it. After the clone, `add`
    /// reads the merged manifest that the workspace would read, and returns
    /// [`AddOutcome::ManifestRefused`] when it fails. Only then does it
    /// write `manifest` to the workspace manifest's path.
    ///
    /// The workspace keeps each host port whose name the merged manifest
    /// declares. When the workspace had one checkout, a port of that
    /// checkout's `mxv.toml` moves to its prefixed name, and every other
    /// port is released, so no port stays held under a name that nothing
    /// declares.
    ///
    /// It writes the record with the new checkout, without a commit, before
    /// it clones, so a run that stops partway leaves a record that names
    /// the unfinished checkout. The same `add` resumes it: it clones
    /// afresh in place of a partial clone, keeps a clone that reached the
    /// workspace's branch, and returns [`AddOutcome::ResumeBlocked`] for a
    /// directory that holds work. Any other `add` returns
    /// [`AddOutcome::Incomplete`]. A failure deletes the new clone and
    /// writes the record, `AGENTS.md` and the workspace manifest as they
    /// were. The caller makes sure that none of the workspace's processes
    /// run, because the names of its units change.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Workspaces::create`].
    pub fn add(
        &self,
        name: &WorkspaceName,
        repo: &Repo,
        mode: Mode,
        manifest: Option<&str>,
    ) -> Result<AddOutcome, Error> {
        let Some(mut workspace) = self.store.read(name)? else {
            return Ok(AddOutcome::NotFound);
        };
        let Some(mirrors) = self
            .mirrors
            .as_ref()
            .filter(|_| workspace.kind == WorkspaceKind::Clone)
        else {
            return Ok(AddOutcome::Worktrees(workspace));
        };
        let repository = self.backend.repository(&repo.path)?;
        let folder = repo
            .folder
            .clone()
            .unwrap_or_else(|| repository.name.clone());
        if let Err(reason) = folder_name(&folder) {
            return Ok(AddOutcome::BadFolder {
                folder,
                repository: repository.root,
                reason,
            });
        }
        let unfinished = workspace
            .checkouts
            .iter()
            .find(|checkout| checkout.commit.is_none())
            .map(|checkout| (checkout.name.clone(), checkout.source.clone()));
        let resumed = match unfinished {
            Some((unfinished, source)) => {
                if unfinished != folder || source != repository.root {
                    return Ok(AddOutcome::Incomplete {
                        folder: unfinished,
                        repository: source,
                        workspace,
                    });
                }
                workspace
                    .checkouts
                    .retain(|checkout| checkout.commit.is_some() || checkout.name != folder);
                true
            }
            None => false,
        };
        let taken = folder_taken(
            &folder,
            workspace
                .checkouts
                .iter()
                .map(|checkout| checkout.name.as_str()),
        ) || (!resumed && Entry::at(&workspace.path.join(&folder))? != Entry::Absent);
        if taken {
            return Ok(AddOutcome::FolderTaken {
                workspace,
                folder,
                repository: repository.root,
            });
        }
        if self
            .backend
            .branch_tip(&repository.root, name.as_str())?
            .is_some()
        {
            return Ok(AddOutcome::BranchTaken {
                repository: repository.root,
                branch: name.to_string(),
            });
        }
        if manifest.is_none()
            && !workspace.has_several_checkouts()
            && workspace.manifest_inputs().workspace.is_some()
            && let Some(first) = workspace.checkouts.first()
        {
            return Ok(AddOutcome::ManifestNeeded {
                manifest: workspace.manifest_file(),
                folder: first.name.clone(),
                workspace,
            });
        }
        let before = workspace.clone();
        let (checkout, url) =
            self.planned_clone(name, &workspace.path.clone(), &repository, &folder, mirrors)?;
        let leftover = if resumed {
            self.leftover(&checkout)?
        } else {
            Leftover::Absent
        };
        if let Leftover::Kept(reason) = leftover {
            let path = checkout.path.clone();
            workspace.checkouts.push(checkout);
            return Ok(AddOutcome::ResumeBlocked {
                workspace,
                path,
                reason,
            });
        }
        workspace.checkouts.push(checkout);
        if mode == Mode::DryRun {
            return Ok(AddOutcome::Planned(workspace));
        }
        self.store.update(&mut workspace)?;
        let finished = matches!(leftover, Leftover::Finished(_));
        let made = self.finish_add(&mut workspace, &before, leftover, &url, manifest);
        match made {
            Ok(Ok(())) => Ok(AddOutcome::Added(workspace)),
            Ok(Err(refused)) => {
                self.undo_add(&workspace, before, finished);
                Ok(AddOutcome::ManifestRefused(refused))
            }
            Err(error) => {
                self.undo_add(&workspace, before, finished);
                Err(error)
            }
        }
    }

    /// Makes the new checkout of `workspace`, its last, from `leftover`,
    /// checks the merged manifest with `manifest`, and writes the workspace
    /// manifest, `AGENTS.md` and the record. `before` is the record without
    /// the new checkout. It returns the merged manifest's error when the
    /// merged manifest fails, having written nothing.
    fn finish_add(
        &self,
        workspace: &mut Workspace,
        before: &Workspace,
        leftover: Leftover,
        url: &str,
        manifest: Option<&str>,
    ) -> Result<Result<(), ManifestError>, Error> {
        let Some(checkout) = workspace.checkouts.last_mut() else {
            return Ok(Ok(()));
        };
        match leftover {
            Leftover::Finished(commit) => checkout.commit = Some(commit),
            Leftover::Debris => {
                delete_clone(&checkout.path)?;
                self.make_clone(checkout, url, &mut 0)?;
            }
            Leftover::Absent | Leftover::Kept(_) => self.make_clone(checkout, url, &mut 0)?,
        }
        let merged = match manifest {
            Some(text) => workspace.merged_manifest_with(text).map(Some),
            None => workspace.manifest(),
        };
        let merged = match merged {
            Ok(merged) => merged,
            Err(refused) => return Ok(Err(refused)),
        };
        workspace.ports = ports_after_add(before, merged.as_ref());
        self.write_agents(workspace)?;
        let path = workspace.manifest_file();
        let previous = manifest.map(|_| fs::read(&path).ok());
        if let Some(text) = manifest {
            store::replace_file(&path, text.as_bytes())?;
        }
        if let Err(error) = self.store.update(workspace) {
            match previous {
                Some(Some(bytes)) => {
                    let _ = store::replace_file(&path, &bytes);
                }
                Some(None) => {
                    let _ = fs::remove_file(&path);
                }
                None => {}
            }
            return Err(error);
        }
        Ok(Ok(()))
    }

    /// Undoes an `add` that failed after it wrote the record with the new
    /// checkout of `workspace`: it writes the workspace manifest and
    /// `AGENTS.md` as `before` had them, deletes the new clone, and writes
    /// the record as `before`. A clone that an earlier run finished, and
    /// that the person may have used, is kept, and so is the record that
    /// names it, so the same `add` resumes it again.
    fn undo_add(&self, workspace: &Workspace, before: Workspace, finished: bool) {
        let _ = self.write_agents(&before);
        if finished {
            return;
        }
        let Some(checkout) = workspace.checkouts.last() else {
            return;
        };
        if delete_clone(&checkout.path).is_ok() {
            let mut before = before;
            let _ = self.store.update(&mut before);
        }
    }

    /// Returns what the directory of `checkout`, an unfinished checkout
    /// that `add` resumes, holds. A directory that holds nothing but
    /// `.git` is debris, because a clone checks out no files until it
    /// reaches the workspace's branch. A clone of its own that is not on
    /// the workspace's branch is debris only when it holds no stash, no
    /// commit that origin lacks, and, unless it holds nothing but `.git`,
    /// no uncommitted change or untracked path, so resuming never deletes
    /// work that someone did in the directory after the run stopped. A
    /// clone on the workspace's branch is finished. Anything else is kept.
    fn leftover(&self, checkout: &Checkout) -> Result<Leftover, Error> {
        let path = &checkout.path;
        match Entry::at(path)? {
            Entry::Absent => return Ok(Leftover::Absent),
            Entry::Link => {
                return Ok(Leftover::Kept(
                    "a symbolic link is at the checkout's path".to_owned(),
                ));
            }
            Entry::Present => {}
        }
        let Ok(entries) = fs::read_dir(path) else {
            return Ok(Leftover::Kept(
                "something other than a directory that mxv can read is at the checkout's path"
                    .to_owned(),
            ));
        };
        let only_git = entries.flatten().all(|entry| entry.file_name() == ".git");
        let own = |head: &crate::backend::Head| {
            head.toplevel
                .as_deref()
                .and_then(|toplevel| toplevel.canonicalize().ok())
                .zip(path.canonicalize().ok())
                .is_some_and(|(toplevel, path)| toplevel == path)
        };
        let head = match self.backend.head(path) {
            Ok(head) if own(&head) => head,
            _ if only_git => return Ok(Leftover::Debris),
            _ => {
                return Ok(Leftover::Kept(
                    "the directory holds files and is not a git clone of its own".to_owned(),
                ));
            }
        };
        if head.branch.as_deref() == Some(checkout.branch.as_str())
            && let Some(commit) = head.commit
        {
            return Ok(Leftover::Finished(commit));
        }
        let unknown = |_| {
            Leftover::Kept(
                "mxv cannot tell whether the clone holds commits or stashes of its own".to_owned(),
            )
        };
        let stashes = match self.backend.stashes(path) {
            Ok(stashes) => stashes,
            Err(error) => return Ok(unknown(error)),
        };
        if !stashes.is_empty() {
            return Ok(Leftover::Kept(format!(
                "the clone holds {} stash entries",
                stashes.len()
            )));
        }
        let unpushed = match self.backend.unpushed_commits(path) {
            Ok(unpushed) => unpushed,
            Err(error) => return Ok(unknown(error)),
        };
        if !unpushed.is_empty() {
            return Ok(Leftover::Kept(format!(
                "the clone holds {} commits that origin does not",
                unpushed.len()
            )));
        }
        // A clone that stopped before it checked out any file holds only
        // `.git`, and git reports every file as deleted there, so only a
        // clone with files of its own needs a clean working tree.
        if only_git {
            return Ok(Leftover::Debris);
        }
        let changes = match self.backend.changes(path) {
            Ok(changes) => changes,
            Err(_) => {
                return Ok(Leftover::Kept(
                    "mxv cannot tell whether the clone holds uncommitted changes".to_owned(),
                ));
            }
        };
        if !changes.changed.is_empty() {
            return Ok(Leftover::Kept(format!(
                "the clone holds {} uncommitted changes or untracked paths",
                changes.changed.len()
            )));
        }
        Ok(Leftover::Debris)
    }

    /// Makes the directory, the clones and the `AGENTS.md` of `workspace`,
    /// whose record the store already holds, and writes the record again
    /// with each checkout's commit. It notes in `made` each thing it
    /// creates. For a workspace of several checkouts, it reads the merged
    /// manifest with `manifest` from the new clones before it writes
    /// `manifest`, and returns the merged manifest's error when it fails,
    /// because the names that `manifest` refers to are those of the clones
    /// as origin holds them.
    fn make_clones(
        &self,
        root: &Path,
        workspace: &mut Workspace,
        urls: &[String],
        manifest: Option<&str>,
        made: &mut Made,
    ) -> Result<Result<(), ManifestError>, Error> {
        let unwritable = |path: &Path| {
            let path = path.to_path_buf();
            move |source| Error::Unwritable { path, source }
        };
        fs::create_dir_all(root).map_err(unwritable(root))?;
        fs::create_dir(&workspace.path).map_err(unwritable(&workspace.path))?;
        made.directory = true;
        for (checkout, url) in workspace.checkouts.iter_mut().zip(urls) {
            self.make_clone(checkout, url, &mut made.clones)?;
        }
        if workspace.has_several_checkouts()
            && let Some(text) = manifest
            && let Err(refused) = workspace.merged_manifest_with(text)
        {
            return Ok(Err(refused));
        }
        made.agents = true;
        self.write_agents(workspace)?;
        write_manifest(workspace, manifest, made)?;
        self.store.update(workspace)?;
        Ok(Ok(()))
    }

    /// Makes the clone of `checkout` from its mirror, fetched from `url`,
    /// on its branch, and sets its commit. It adds one to `clones` once it
    /// starts the clone, which may leave a directory behind.
    fn make_clone(
        &self,
        checkout: &mut Checkout,
        url: &str,
        clones: &mut usize,
    ) -> Result<(), Error> {
        let Some(mirror) = checkout.mirror.clone() else {
            return Ok(());
        };
        let default = checkout
            .base
            .strip_prefix("refs/remotes/origin/")
            .unwrap_or(&checkout.base)
            .to_owned();
        {
            let _lock = self.lock_mirror(&mirror)?;
            self.backend.prepare_mirror(&mirror)?;
            self.backend.fetch_mirror(&mirror, url, &default)?;
            *clones += 1;
            self.backend.clone_mirror(&mirror, &checkout.path, url)?;
        }
        self.copy_excludes(checkout)?;
        if self
            .backend
            .create_branch(&checkout.path, &checkout.branch, &checkout.base)?
            .is_none()
        {
            return Err(Error::CommandFailed {
                operation: "update-ref",
                dir: checkout.path.clone(),
                code: None,
                message: format!(
                    "the new clone already holds a branch named {}",
                    checkout.branch
                ),
            });
        }
        let commit = self
            .backend
            .switch_branch(&checkout.path, &checkout.branch)?;
        checkout.commit = Some(commit);
        Ok(())
    }

    /// Writes the `AGENTS.md` of `workspace`, which names the origin of each
    /// checkout as the checkout's own `origin` holds it, without a user
    /// name or password.
    fn write_agents(&self, workspace: &Workspace) -> Result<(), Error> {
        let origins: Vec<String> = workspace
            .checkouts
            .iter()
            .map(|checkout| {
                self.backend
                    .origin_url(&checkout.path)
                    .map(|url| crate::mirror::redacted(&url))
            })
            .collect::<Result<_, _>>()?;
        store::replace_file(
            &workspace.agents_file(),
            agents_text(workspace, &origins).as_bytes(),
        )
    }

    /// Copies the `info/exclude` file of the person's repository into the
    /// clone of `checkout`, so a path that the person ignores there, such as
    /// `.env`, stays ignored in the clone, as it was in a worktree, which
    /// shared the file. A repository without the file copies nothing.
    fn copy_excludes(&self, checkout: &Checkout) -> Result<(), Error> {
        let from = self.backend.exclude_file(&checkout.source)?;
        let rules = match fs::read(&from) {
            Ok(rules) => rules,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(source) => return Err(Error::StateUnreadable { path: from, source }),
        };
        let to = self.backend.exclude_file(&checkout.path)?;
        if let Some(dir) = to.parent() {
            fs::create_dir_all(dir).map_err(|source| Error::Unwritable {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        store::replace_file(&to, &rules)
    }

    /// Takes the lock of the mirror at `mirror`, which sits beside it as
    /// `<mirror>.lock`, waiting up to `mirror_wait` for another command to
    /// release it.
    fn lock_mirror(&self, mirror: &Path) -> Result<store::Lock, Error> {
        let mut lock = mirror.as_os_str().to_owned();
        lock.push(".lock");
        let busy = |error| match error {
            Error::Busy { .. } => Error::MirrorBusy {
                mirror: mirror.to_path_buf(),
            },
            other => other,
        };
        match store::lock(Path::new(&lock), Duration::ZERO) {
            Err(Error::Busy { .. }) => {
                if let Some(notice) = self.mirror_notice {
                    notice(mirror, self.mirror_wait);
                }
                store::lock(Path::new(&lock), self.mirror_wait).map_err(busy)
            }
            other => other.map_err(busy),
        }
    }

    /// Makes the directory, the checkouts and the `AGENTS.md` of
    /// `workspace`, whose record the store already holds, and writes the
    /// record again with each checkout's commit. It creates each branch in
    /// one step that fails when the branch exists, and then adds the
    /// checkout on it. It notes in `made` each thing it creates.
    fn make(
        &self,
        root: &Path,
        workspace: &mut Workspace,
        manifest: Option<&str>,
        made: &mut Made,
    ) -> Result<Making, Error> {
        let unwritable = |path: &Path| {
            let path = path.to_path_buf();
            move |source| Error::Unwritable { path, source }
        };
        fs::create_dir_all(root).map_err(unwritable(root))?;
        fs::create_dir(&workspace.path).map_err(unwritable(&workspace.path))?;
        made.directory = true;
        for checkout in &mut workspace.checkouts {
            let Some(start) =
                self.backend
                    .create_branch(&checkout.source, &checkout.branch, &checkout.base)?
            else {
                return Ok(Making::BranchTaken {
                    repository: checkout.source.clone(),
                    branch: checkout.branch.clone(),
                });
            };
            made.branches.push(start);
            let commit =
                self.backend
                    .add_checkout(&checkout.source, &checkout.path, &checkout.branch)?;
            checkout.commit = Some(commit);
        }
        made.agents = true;
        self.write_agents(workspace)?;
        write_manifest(workspace, manifest, made)?;
        self.store.update(workspace)?;
        Ok(Making::Done)
    }

    /// Deletes what a failed [`Workspaces::make`] created, as `made`
    /// records it: each checkout it started, even one that `worktree add`
    /// left locked or half made, each branch it created while that branch
    /// still points where the run created it, its `AGENTS.md` and its
    /// directory. A branch that another process created is never touched.
    /// Each step is best effort, because the caller reports the failure
    /// that stopped the creation. The record is deleted only when every
    /// step succeeded, so `mxv remove` can finish a cleanup that failed.
    fn undo(&self, workspace: &Workspace, made: &Made) {
        let mut clean = true;
        for checkout in workspace.checkouts.iter().take(made.clones) {
            clean &= delete_clone(&checkout.path).is_ok();
        }
        for (checkout, start) in workspace.checkouts.iter().zip(&made.branches) {
            if let Ok(Some(listed)) = self
                .backend
                .listed_checkout(&checkout.source, &checkout.path)
            {
                if listed.locked.as_deref() == Some(INITIALIZING) {
                    clean &= self
                        .backend
                        .unlock_checkout(&checkout.source, &checkout.path)
                        .is_ok();
                }
                clean &= self
                    .backend
                    .remove_checkout(&checkout.source, &checkout.path, true)
                    .is_ok();
            }
            clean &= self.backend.prune_checkouts(&checkout.source).is_ok();
            clean &= self
                .backend
                .delete_branch_at(&checkout.source, &checkout.branch, start)
                .is_ok();
        }
        if made.manifest {
            clean &= match fs::remove_file(workspace.manifest_file()) {
                Ok(()) => true,
                Err(error) => error.kind() == io::ErrorKind::NotFound,
            };
        }
        if made.agents {
            let agents = workspace.agents_file();
            clean &= match fs::remove_file(&agents) {
                Ok(()) => true,
                Err(error) => error.kind() == io::ErrorKind::NotFound,
            };
        }
        if made.directory {
            clean &= fs::remove_dir(&workspace.path).is_ok();
        }
        if clean {
            let _ = self.store.delete(&workspace.name);
        }
    }

    /// Returns what removing the workspace `name` would do, or why it
    /// cannot be removed. It changes nothing.
    ///
    /// The removal stops, unless `allow` permits the loss, when
    ///
    /// - a checkout has uncommitted changes or untracked files
    ///   ([`RemovalPlan::Dirty`]), or ignored files outside [`DISPOSABLE`]
    ///   ([`RemovalPlan::IgnoredFiles`]), and `allow.discard_changes` is
    ///   not set;
    /// - a checkout's `HEAD` holds commits that no branch holds
    ///   ([`RemovalPlan::StrayCommits`]), and `allow.delete_unmerged` is not
    ///   set.
    ///
    /// A branch whose commits are all on its base is deleted. A branch
    /// with other commits is deleted only when `allow.delete_unmerged` is
    /// set, and kept otherwise. The comparison reads the base as the
    /// repository last fetched it, so a branch that was merged by
    /// squashing, or merged after the last fetch, reads as unmerged.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Store::read`] and of the backend's status
    /// and branch commands.
    pub fn plan_removal(&self, name: &WorkspaceName, allow: Allow) -> Result<RemovalPlan, Error> {
        let Some(workspace) = self.store.read(name)? else {
            return Ok(RemovalPlan::NotFound);
        };
        let directory = match Entry::at(&workspace.path)? {
            Entry::Link => {
                return Ok(RemovalPlan::Unowned {
                    checkout: workspace.path.clone(),
                    workspace,
                });
            }
            Entry::Present => Fate::Removed,
            Entry::Absent => Fate::Missing,
        };
        let mut checkouts = Vec::new();
        let mut sorted = Sorted::default();
        for checkout in &workspace.checkouts {
            if workspace.kind == WorkspaceKind::Clone {
                match self.plan_clone(&workspace, checkout, allow, &mut sorted)? {
                    Ok(planned) => checkouts.push(planned),
                    Err(refusal) => return Ok(refusal.plan(workspace)),
                }
                continue;
            }
            let source = checkout.source.is_dir();
            let entry = Entry::at(&checkout.path)?;
            if entry == Entry::Link {
                return Ok(RemovalPlan::Unowned {
                    checkout: checkout.path.clone(),
                    workspace,
                });
            }
            let listed = if source {
                self.backend
                    .listed_checkout(&checkout.source, &checkout.path)?
            } else {
                None
            };
            if entry == Entry::Present && listed.is_none() {
                return Ok(RemovalPlan::Unowned {
                    checkout: checkout.path.clone(),
                    workspace,
                });
            }
            let mut unlock = false;
            if let Some(reason) = listed.as_ref().and_then(|listed| listed.locked.clone()) {
                if reason != INITIALIZING {
                    return Ok(RemovalPlan::Locked {
                        checkout: checkout.path.clone(),
                        reason,
                        workspace,
                    });
                }
                unlock = true;
            }
            let partial = entry == Entry::Present && is_partial(checkout)?;
            if partial && !allow.discard_changes {
                let checkout = checkout.name.clone();
                return Ok(partial_refusal(workspace, &checkout));
            }
            let mut disposable = Vec::new();
            let stray = if entry == Entry::Present && !partial {
                disposable = self.sort(&workspace, checkout, &mut sorted)?;
                self.backend.stray_commits(&checkout.path, "HEAD")?
            } else {
                // The directory is gone, but until a prune the repository
                // still holds the checkout's `HEAD`, which may name commits
                // that no branch holds.
                match listed.as_ref().and_then(|listed| listed.head.as_deref()) {
                    Some(head) => self.backend.stray_commits(&checkout.source, head)?,
                    None => 0,
                }
            };
            if stray > 0 && !allow.delete_unmerged {
                return Ok(RemovalPlan::StrayCommits {
                    checkout: checkout.path.clone(),
                    commits: stray,
                    workspace,
                });
            }
            let directory = if entry == Entry::Present {
                Fate::Removed
            } else {
                Fate::Missing
            };
            let tip = if source {
                self.backend
                    .branch_tip(&checkout.source, &checkout.branch)?
            } else {
                None
            };
            let branch = match &tip {
                None => BranchFate::Missing,
                Some(tip) => {
                    match self
                        .backend
                        .unmerged_commits(&checkout.source, tip, &checkout.base)?
                    {
                        Some(0) => BranchFate::Deleted,
                        commits if allow.delete_unmerged => BranchFate::DeletedUnmerged { commits },
                        commits => BranchFate::KeptUnmerged { commits },
                    }
                }
            };
            checkouts.push(CheckoutRemoval {
                checkout: checkout.clone(),
                directory,
                branch,
                tip,
                stray_commits: stray,
                lost: Vec::new(),
                stashes: Vec::new(),
                unlock,
                disposable,
                partial,
            });
        }
        let Sorted {
            mut changes,
            mut ignored,
            disposables,
            warnings,
            manifest,
        } = sorted;
        if !allow.discard_changes {
            if !changes.is_empty() {
                return Ok(RemovalPlan::Dirty { workspace, changes });
            }
            if !ignored.is_empty() {
                return Ok(RemovalPlan::IgnoredFiles {
                    workspace,
                    paths: ignored,
                    warnings,
                });
            }
        }
        changes.append(&mut ignored);
        let workspace_manifest =
            Some(workspace.manifest_file()).filter(|path| fs::symlink_metadata(path).is_ok());
        Ok(RemovalPlan::Ready(Removal {
            workspace,
            checkouts,
            directory,
            allow,
            disposable: disposables,
            discarded: changes,
            manifest,
            warnings,
            workspace_manifest,
        }))
    }

    /// Reads the changes of the checkout `checkout`, whose directory
    /// exists, and adds each to `sorted`: a change, a disposable path, a
    /// path that only the manifest makes disposable, or another ignored
    /// path. It returns the checkout's disposable paths, relative to it.
    fn sort(
        &self,
        workspace: &Workspace,
        checkout: &Checkout,
        sorted: &mut Sorted,
    ) -> Result<Vec<String>, Error> {
        let found = self.backend.changes(&checkout.path)?;
        let (set, warning) = self.disposable_set(workspace, checkout);
        sorted.warnings.extend(warning);
        let named = |item: &String| format!("{}: {item}", checkout.name);
        sorted.changes.extend(found.changed.iter().map(named));
        let mut own = Vec::new();
        for path in &found.ignored {
            // A directory that holds a `.git` entry may hold a repository
            // with unpushed work, so it is never deleted without the
            // caller's leave.
            let source = set.source(path);
            if source.is_some() && crate::disk::keep_reason(&checkout.path, path).is_none() {
                if source == Some(DisposableSource::Manifest) {
                    sorted.manifest.push(named(path));
                }
                sorted.disposables.push(named(path));
                own.push(path.clone());
            } else {
                sorted.ignored.push(named(path));
            }
        }
        Ok(own)
    }

    /// Plans the removal of `checkout`, a clone, for
    /// [`Workspaces::plan_removal`], and adds its changes to `sorted`.
    ///
    /// A clone's branch and commits live only in its own `.git`, so
    /// deleting the clone loses each commit that `origin` does not hold.
    /// Unless `allow` permits deleting unmerged commits, the plan first
    /// fetches the clone's `origin`, deleting the remote branches that
    /// `origin` no longer has, and then refuses a clone with commits that
    /// no remote branch holds. It refuses a clone with stashes unless
    /// `allow` permits discarding changes. A directory whose `.git` is not
    /// a directory of its own is not the workspace's clone.
    fn plan_clone(
        &self,
        workspace: &Workspace,
        checkout: &Checkout,
        allow: Allow,
        sorted: &mut Sorted,
    ) -> Result<Result<CheckoutRemoval, CloneRefusal>, Error> {
        let entry = Entry::at(&checkout.path)?;
        let refused = |refusal| Ok(Err(refusal));
        let mut planned = CheckoutRemoval {
            checkout: checkout.clone(),
            directory: Fate::Missing,
            branch: BranchFate::Missing,
            tip: None,
            stray_commits: 0,
            lost: Vec::new(),
            stashes: Vec::new(),
            unlock: false,
            disposable: Vec::new(),
            partial: false,
        };
        match entry {
            Entry::Link => return refused(CloneRefusal::Unowned(checkout.path.clone())),
            Entry::Absent => return Ok(Ok(planned)),
            Entry::Present => {}
        }
        let dot_git = checkout.path.join(".git");
        let owned = Entry::at(&dot_git)? == Entry::Present && dot_git.is_dir();
        if !owned {
            return refused(CloneRefusal::Unowned(checkout.path.clone()));
        }
        if !allow.delete_unmerged {
            self.backend.refresh_origin(&checkout.path)?;
        }
        planned.disposable = self.sort(workspace, checkout, sorted)?;
        let stashes = self.backend.stashes(&checkout.path)?;
        if !stashes.is_empty() && !allow.discard_changes {
            return refused(CloneRefusal::Stashed {
                checkout: checkout.path.clone(),
                stashes: count(&stashes),
            });
        }
        let lost = self.backend.unpushed_commits(&checkout.path)?;
        let unpushed = count(&lost);
        if unpushed > 0 && !allow.delete_unmerged {
            return refused(CloneRefusal::Unpushed {
                checkout: checkout.path.clone(),
                commits: unpushed,
            });
        }
        planned.directory = Fate::Removed;
        planned.stray_commits = unpushed;
        planned.lost = lost;
        planned.stashes = stashes;
        planned.branch = if unpushed == 0 {
            BranchFate::Deleted
        } else {
            BranchFate::DeletedUnmerged {
                commits: Some(unpushed),
            }
        };
        Ok(Ok(planned))
    }

    /// Deletes the disposable directories `paths` of `checkout` by file
    /// descriptor, with no deadline, before git or mxv deletes the rest of
    /// the checkout. Each path must still pass the checks that `mxv clean`
    /// makes after git's report. A path that fails them, such as one that
    /// is gone or now holds a nested repository, is left to the deletion
    /// of the checkout, which deletes or refuses it as before.
    fn delete_disposable(&self, checkout: &Checkout, paths: &[String]) -> Result<(), Error> {
        for path in paths {
            if self.undeletable(checkout, path)?.is_some() {
                continue;
            }
            crate::disk::remove_tree(&checkout.path, path)
                .map_err(|(path, source)| Error::Unwritable { path, source })?;
        }
        Ok(())
    }

    /// Carries out `confirmed`, which [`Workspaces::plan_removal`] returned
    /// and the caller confirmed. It plans the removal again first, because
    /// the workspace may have changed while the caller confirmed, and
    /// returns [`RemovalOutcome::Changed`] without removing anything when
    /// the new plan refuses the removal, or would lose commits that no
    /// branch holds or discard changes that `confirmed` did not show.
    /// Otherwise it removes each checkout, deletes each branch that the
    /// plan deletes while the branch still points at the commit that the
    /// plan saw, removes the workspace's `AGENTS.md` and directory, and
    /// deletes the record. A branch that moved since the plan stays, as
    /// [`BranchFate::KeptMoved`]. The workspace's directory stays when it
    /// still holds other files. It returns what it did.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Workspaces::plan_removal`], the failures of
    /// the backend's removal commands, and [`Error::Unwritable`] when
    /// `AGENTS.md`, the directory or the record cannot be removed. The
    /// steps before the failure stay done, and the record stays, so the
    /// caller can run the removal again.
    ///
    /// Each checkout's [`CheckoutRemoval::disposable`] directories are
    /// deleted first, by file descriptor and with no deadline, so a large
    /// `target/` never runs into the deadline of `git worktree remove`. A
    /// clone is moved aside before it is deleted, and what an earlier
    /// removal left aside is deleted first, so a removal that stopped
    /// partway is finished by the next one. A worktree that is
    /// [`CheckoutRemoval::partial`] is deleted by file descriptor and then
    /// pruned from its repository.
    pub fn remove(&self, confirmed: Removal) -> Result<RemovalOutcome, Error> {
        let mut removal = match self.plan_removal(&confirmed.workspace.name, confirmed.allow)? {
            RemovalPlan::Ready(fresh) if !loses_more(&fresh, &confirmed) => fresh,
            plan => return Ok(RemovalOutcome::Changed(plan)),
        };
        for (planned, seen) in removal.checkouts.iter_mut().zip(&confirmed.checkouts) {
            let checkout = &planned.checkout;
            // What an interrupted deletion of a clone left aside goes first.
            if let Some(aside) = aside(&checkout.path) {
                delete_directory(&aside)?;
            }
            if planned.directory == Fate::Removed && !planned.partial {
                self.delete_disposable(checkout, &planned.disposable)?;
            }
            if removal.workspace.kind == WorkspaceKind::Clone {
                // The clone holds its branch, so deleting the clone deletes
                // the branch, and nothing of the person's repository changes.
                if planned.directory == Fate::Removed {
                    delete_clone(&checkout.path)?;
                }
                planned.branch = seen.branch;
                continue;
            }
            if planned.unlock {
                self.backend
                    .unlock_checkout(&checkout.source, &checkout.path)?;
            }
            match planned.directory {
                Fate::Removed if planned.partial => {
                    delete_directory(&checkout.path)?;
                    if checkout.source.is_dir() {
                        self.backend.prune_checkouts(&checkout.source)?;
                    }
                }
                Fate::Removed => {
                    self.backend.remove_checkout(
                        &checkout.source,
                        &checkout.path,
                        removal.allow.discard_changes,
                    )?;
                }
                Fate::Missing | Fate::Kept => {
                    if checkout.source.is_dir() {
                        self.backend.prune_checkouts(&checkout.source)?;
                    }
                }
            }
            // The branch's fate is the one that the caller confirmed, at
            // the commit that the caller saw.
            planned.branch = seen.branch;
            planned.tip.clone_from(&seen.tip);
            let deletes = matches!(
                planned.branch,
                BranchFate::Deleted | BranchFate::DeletedUnmerged { .. }
            );
            let Some(tip) = planned.tip.as_deref().filter(|_| deletes) else {
                continue;
            };
            if let Err(error) =
                self.backend
                    .delete_branch_at(&checkout.source, &checkout.branch, tip)
            {
                planned.branch = match self
                    .backend
                    .branch_tip(&checkout.source, &checkout.branch)?
                {
                    None => BranchFate::Missing,
                    Some(now) if now != tip => BranchFate::KeptMoved,
                    Some(_) => return Err(error),
                };
            }
        }
        let manifest = removal.workspace.manifest_file();
        match fs::remove_file(&manifest) {
            Ok(()) => removal.workspace_manifest = Some(manifest),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                removal.workspace_manifest = None;
            }
            Err(source) => {
                return Err(Error::Unwritable {
                    path: manifest,
                    source,
                });
            }
        }
        let agents = removal.workspace.agents_file();
        match fs::remove_file(&agents) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(Error::Unwritable {
                    path: agents,
                    source,
                });
            }
        }
        removal.directory = match fs::remove_dir(&removal.workspace.path) {
            Ok(()) => Fate::Removed,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Fate::Missing,
            Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => Fate::Kept,
            Err(source) => {
                return Err(Error::Unwritable {
                    path: removal.workspace.path.clone(),
                    source,
                });
            }
        };
        self.store.delete(&removal.workspace.name)?;
        Ok(RemovalOutcome::Done(removal))
    }
}

/// Returns the number of `items`, which is never more than a `u32` holds in
/// practice, and saturates when it is.
fn count(items: &[String]) -> u32 {
    u32::try_from(items.len()).unwrap_or(u32::MAX)
}

/// Returns the host ports that a workspace holds once it gains a checkout
/// and reads `merged`, where `before` is its record without the new
/// checkout. It keeps each port whose name `merged` declares. When
/// `before` had one checkout, a port of that checkout's `mxv.toml` moves
/// to its prefixed name, when `merged` declares that name and `before`
/// holds no port under it. Every other port is released, so no port stays
/// held under a name that nothing declares. The numbers stay as they were,
/// and `mxv up` allocates a number only for a name that holds none.
fn ports_after_add(
    before: &Workspace,
    merged: Option<&Manifest>,
) -> std::collections::BTreeMap<String, u16> {
    let declared =
        |name: &str| merged.is_some_and(|merged| merged.ports.iter().any(|port| port == name));
    let prefix = (!before.has_several_checkouts())
        .then(|| before.checkouts.first())
        .flatten()
        .map(|checkout| checkout_prefix(&checkout.name));
    let mut ports = std::collections::BTreeMap::new();
    for (name, number) in &before.ports {
        if declared(name) {
            ports.insert(name.clone(), *number);
        } else if let Some(prefix) = &prefix {
            let moved = prefixed(prefix, name);
            if declared(&moved) && !before.ports.contains_key(&moved) {
                ports.insert(moved, *number);
            }
        }
    }
    ports
}

/// Writes `manifest`, when it is given, as the workspace manifest of
/// `workspace`, and notes in `made` that the run wrote it.
fn write_manifest(
    workspace: &Workspace,
    manifest: Option<&str>,
    made: &mut Made,
) -> Result<(), Error> {
    if let Some(text) = manifest {
        made.manifest = true;
        store::replace_file(&workspace.manifest_file(), text.as_bytes())?;
    }
    Ok(())
}
