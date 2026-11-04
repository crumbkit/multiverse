//! The workspace operations: create, list, find and remove.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::backend::{Backend, INITIALIZING};
use crate::disposable::{DisposableSet, PathGlob};
use crate::error::Error;
use crate::manifest::Manifest;
use crate::name::WorkspaceName;
use crate::record::{Checkout, Workspace, agents_text};
use crate::store::{self, Created, Records, Store};

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
    /// The repository already has a local branch with the workspace's name.
    BranchTaken {
        /// The repository's top-level directory.
        repository: PathBuf,
        /// The branch's name.
        branch: String,
    },
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
    /// removal loses.
    pub stray_commits: u32,
    /// Whether the removal unlocks the checkout first, because a killed
    /// `git worktree add` left it locked with the reason
    /// [`INITIALIZING`](crate::backend::INITIALIZING).
    pub unlock: bool,
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
/// `.env`, an ignored file named `build`, and an ignored file inside a
/// tracked `dist/` directory, such as `tools/dist/prod.secret`.
pub const DISPOSABLE: &[&str] = &["target", "node_modules", "dist", "build", ".cache"];

/// Reports whether `fresh`, a new plan of the removal that the caller
/// confirmed as `confirmed`, would lose more than `confirmed` showed: a
/// different record, more commits that no branch holds in a checkout, or a
/// change or ignored path to discard that `confirmed` did not list.
fn loses_more(fresh: &Removal, confirmed: &Removal) -> bool {
    fresh.workspace != confirmed.workspace
        || fresh
            .checkouts
            .iter()
            .zip(&confirmed.checkouts)
            .any(|(now, seen)| now.stray_commits > seen.stray_commits)
        || fresh
            .discarded
            .iter()
            .any(|path| !confirmed.discarded.contains(path))
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
    /// A checkout is locked for a reason other than
    /// [`INITIALIZING`](crate::backend::INITIALIZING), so something else may
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
        }
    }

    /// Returns the workspaces with `globs`, the globs of disposable paths
    /// that the person declares, added to the built-in [`DISPOSABLE`] set
    /// and to each repository's own globs.
    #[must_use]
    pub fn with_disposable(mut self, globs: Vec<PathGlob>) -> Self {
        self.disposable = globs;
        self
    }

    /// Returns the disposable set of `checkout`: the built-in
    /// [`DISPOSABLE`] directories, the globs that the checkout's own
    /// `mxv.toml` declares, and the person's globs. A manifest that cannot
    /// be read adds no globs, so fewer paths are disposable.
    pub(crate) fn disposable_set(&self, checkout: &Checkout) -> DisposableSet {
        let declared = Manifest::read(&checkout.path)
            .ok()
            .flatten()
            .map(|manifest| manifest.disposable)
            .unwrap_or_default();
        DisposableSet::built_in()
            .with(&declared)
            .with(&self.disposable)
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
    /// one checkout of the repository at `repository`, on a new branch `name` from the freshly fetched
    /// default branch of `origin`.
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
    /// fetch and the checkout, and [`Error::Unwritable`] when a directory,
    /// the record or `AGENTS.md` cannot be written.
    pub fn create(
        &self,
        root: &Path,
        name: &WorkspaceName,
        repository: &Path,
        mode: Mode,
    ) -> Result<NewOutcome, Error> {
        if let Some(existing) = self.store.read(name)? {
            return Ok(NewOutcome::NameTaken(existing));
        }
        let repository = self.backend.repository(repository)?;
        let path = root.join(name.as_str());
        if Entry::at(&path)? != Entry::Absent {
            return Ok(NewOutcome::PathTaken(path));
        }
        if self
            .backend
            .branch_tip(&repository.root, name.as_str())?
            .is_some()
        {
            return Ok(NewOutcome::BranchTaken {
                repository: repository.root,
                branch: name.to_string(),
            });
        }
        if mode == Mode::Change {
            self.backend.fetch(&repository.root)?;
        }
        let base = self.backend.base(&repository.root)?;
        let checkout = Checkout::new(
            repository.name.clone(),
            &repository.root,
            path.join(&repository.name),
            name.to_string(),
            base,
        );
        let mut workspace = Workspace::new(name.clone(), path, vec![checkout]);
        if mode == Mode::DryRun {
            return Ok(NewOutcome::Planned(workspace));
        }
        if self.store.create(&workspace)? == Created::Taken {
            return Ok(self.store.read(name)?.map_or(
                NewOutcome::PathTaken(workspace.path.clone()),
                NewOutcome::NameTaken,
            ));
        }
        let mut made = Made::default();
        match self.make(root, &mut workspace, &mut made) {
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

    /// Makes the directory, the checkouts and the `AGENTS.md` of
    /// `workspace`, whose record the store already holds, and writes the
    /// record again with each checkout's commit. It creates each branch in
    /// one step that fails when the branch exists, and then adds the
    /// checkout on it. It notes in `made` each thing it creates.
    fn make(
        &self,
        root: &Path,
        workspace: &mut Workspace,
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
        store::replace_file(&workspace.agents_file(), agents_text(workspace).as_bytes())?;
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
        let mut changes = Vec::new();
        let mut ignored = Vec::new();
        let mut disposables = Vec::new();
        for checkout in &workspace.checkouts {
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
            let stray = if entry == Entry::Present {
                let found = self.backend.changes(&checkout.path)?;
                let set = self.disposable_set(checkout);
                let named = |item: &String| format!("{}: {item}", checkout.name);
                changes.extend(found.changed.iter().map(named));
                for path in &found.ignored {
                    if set.matches(path) {
                        disposables.push(named(path));
                    } else {
                        ignored.push(named(path));
                    }
                }
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
                unlock,
            });
        }
        if !allow.discard_changes {
            if !changes.is_empty() {
                return Ok(RemovalPlan::Dirty { workspace, changes });
            }
            if !ignored.is_empty() {
                return Ok(RemovalPlan::IgnoredFiles {
                    workspace,
                    paths: ignored,
                });
            }
        }
        changes.append(&mut ignored);
        Ok(RemovalPlan::Ready(Removal {
            workspace,
            checkouts,
            directory,
            allow,
            disposable: disposables,
            discarded: changes,
        }))
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
    pub fn remove(&self, confirmed: Removal) -> Result<RemovalOutcome, Error> {
        let mut removal = match self.plan_removal(&confirmed.workspace.name, confirmed.allow)? {
            RemovalPlan::Ready(fresh) if !loses_more(&fresh, &confirmed) => fresh,
            plan => return Ok(RemovalOutcome::Changed(plan)),
        };
        for (planned, seen) in removal.checkouts.iter_mut().zip(&confirmed.checkouts) {
            let checkout = &planned.checkout;
            if planned.unlock {
                self.backend
                    .unlock_checkout(&checkout.source, &checkout.path)?;
            }
            match planned.directory {
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
