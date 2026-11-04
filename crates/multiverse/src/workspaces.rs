//! The workspace operations: create, list, find and remove.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::backend::Backend;
use crate::error::Error;
use crate::name::WorkspaceName;
use crate::record::{Checkout, Workspace, agents_text};
use crate::store::{self, Created, Store};

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
/// passes to [`Workspaces::create`]. A workspace named `reading-list` of the repository `inbox` lives at
/// `<root>/reading-list`. That directory holds the workspace's `AGENTS.md`
/// and the checkout `<root>/reading-list/inbox`, so git never sees the
/// `AGENTS.md`, and a later workspace can hold one checkout for each of
/// several repositories beside it.
#[derive(Debug)]
pub struct Workspaces<B> {
    /// The store of workspace records.
    store: Store,
    /// The backend that makes and removes checkouts.
    backend: B,
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
    /// The branch is deleted, because its commits are on its base or the
    /// caller forced the removal.
    Deleted,
    /// The branch stays, because it holds commits that are not on its base.
    KeptUnmerged,
    /// The branch, or its repository, was already gone.
    Missing,
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
    /// Whether the caller forced the removal.
    pub forced: bool,
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
        /// checkout's directory name in front, such as `inbox: ?? notes.md`.
        changes: Vec<String>,
    },
    /// A checkout's directory exists, but its repository does not list it
    /// as a checkout, so the directory is not the workspace's to remove
    /// (DUR-4).
    Unowned {
        /// The workspace's record.
        workspace: Workspace,
        /// The directory that the record names.
        checkout: PathBuf,
    },
}

impl<B: Backend> Workspaces<B> {
    /// Returns the workspaces with their records in `store` and their
    /// checkouts made by `backend`.
    #[must_use]
    pub const fn new(store: Store, backend: B) -> Self {
        Self { store, backend }
    }

    /// Returns every workspace, in name order.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Store::list`].
    pub fn list(&self) -> Result<Vec<Workspace>, Error> {
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
        if path.exists() {
            return Ok(NewOutcome::PathTaken(path));
        }
        if self.backend.has_branch(&repository.root, name.as_str())? {
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
        match self.make(root, &mut workspace) {
            Ok(()) => Ok(NewOutcome::Created(workspace)),
            Err(error) => {
                self.undo(&workspace);
                Err(error)
            }
        }
    }

    /// Makes the directory, the checkouts and the `AGENTS.md` of
    /// `workspace`, whose record the store already holds, and writes the
    /// record again with each checkout's commit.
    fn make(&self, root: &Path, workspace: &mut Workspace) -> Result<(), Error> {
        let unwritable = |path: &Path| {
            let path = path.to_path_buf();
            move |source| Error::Unwritable { path, source }
        };
        fs::create_dir_all(root).map_err(unwritable(root))?;
        fs::create_dir(&workspace.path).map_err(unwritable(&workspace.path))?;
        for checkout in &mut workspace.checkouts {
            let commit = self.backend.add_checkout(
                &checkout.source,
                &checkout.path,
                &checkout.branch,
                &checkout.base,
            )?;
            checkout.commit = Some(commit);
        }
        store::replace_file(&workspace.agents_file(), agents_text(workspace).as_bytes())?;
        self.store.update(workspace)
    }

    /// Deletes what a failed [`Workspaces::make`] left: the checkouts it
    /// made, their branches, the workspace's directory and the record. Each
    /// step is best effort, because the caller reports the failure that
    /// stopped the creation.
    fn undo(&self, workspace: &Workspace) {
        for checkout in &workspace.checkouts {
            if checkout.commit.is_some() {
                let _ = self
                    .backend
                    .remove_checkout(&checkout.source, &checkout.path, true);
                let _ = self
                    .backend
                    .delete_branch(&checkout.source, &checkout.branch);
            }
        }
        let _ = fs::remove_file(workspace.agents_file());
        let _ = fs::remove_dir(&workspace.path);
        let _ = self.store.delete(&workspace.name);
    }

    /// Returns what removing the workspace `name` would do, or why it
    /// cannot be removed. It changes nothing.
    ///
    /// A checkout with uncommitted or untracked changes stops the removal
    /// unless `force` is set. A branch is deleted when its commits are on
    /// its base, or when `force` is set, and kept otherwise. The comparison
    /// reads the base as the repository last fetched it, so a branch that
    /// was merged by squashing, or merged after the last fetch, reads as
    /// unmerged.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Store::read`] and of the backend's status
    /// and branch commands.
    pub fn plan_removal(&self, name: &WorkspaceName, force: bool) -> Result<RemovalPlan, Error> {
        let Some(workspace) = self.store.read(name)? else {
            return Ok(RemovalPlan::NotFound);
        };
        let mut checkouts = Vec::new();
        let mut changes = Vec::new();
        for checkout in &workspace.checkouts {
            let source = checkout.source.is_dir();
            let directory = if checkout.path.exists() {
                if !source
                    || !self
                        .backend
                        .owns_checkout(&checkout.source, &checkout.path)?
                {
                    return Ok(RemovalPlan::Unowned {
                        checkout: checkout.path.clone(),
                        workspace,
                    });
                }
                for change in self.backend.changes(&checkout.path)? {
                    changes.push(format!("{}: {change}", checkout.name));
                }
                Fate::Removed
            } else {
                Fate::Missing
            };
            let branch = if !source
                || !self
                    .backend
                    .has_branch(&checkout.source, &checkout.branch)?
            {
                BranchFate::Missing
            } else if force
                || self
                    .backend
                    .is_merged(&checkout.source, &checkout.branch, &checkout.base)?
            {
                BranchFate::Deleted
            } else {
                BranchFate::KeptUnmerged
            };
            checkouts.push(CheckoutRemoval {
                checkout: checkout.clone(),
                directory,
                branch,
            });
        }
        if !changes.is_empty() && !force {
            return Ok(RemovalPlan::Dirty { workspace, changes });
        }
        let directory = if workspace.path.exists() {
            Fate::Removed
        } else {
            Fate::Missing
        };
        Ok(RemovalPlan::Ready(Removal {
            workspace,
            checkouts,
            directory,
            forced: force,
        }))
    }

    /// Carries out `removal`, which [`Workspaces::plan_removal`] returned:
    /// it removes each checkout, deletes each branch that the plan deletes,
    /// removes the workspace's `AGENTS.md` and directory, and deletes the
    /// record. It returns what it did. The workspace's directory stays when
    /// it still holds other files, and the result says so.
    ///
    /// # Errors
    ///
    /// Returns the failures of the backend's removal commands, and
    /// [`Error::Unwritable`] when `AGENTS.md`, the directory or the record
    /// cannot be removed. The steps before the failure stay done, and the
    /// record stays, so the caller can run the removal again.
    pub fn remove(&self, mut removal: Removal) -> Result<Removal, Error> {
        for planned in &removal.checkouts {
            let checkout = &planned.checkout;
            match planned.directory {
                Fate::Removed => {
                    self.backend.remove_checkout(
                        &checkout.source,
                        &checkout.path,
                        removal.forced,
                    )?;
                }
                Fate::Missing | Fate::Kept => {
                    if checkout.source.is_dir() {
                        self.backend.prune_checkouts(&checkout.source)?;
                    }
                }
            }
            if planned.branch == BranchFate::Deleted {
                self.backend
                    .delete_branch(&checkout.source, &checkout.branch)?;
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
        Ok(removal)
    }
}
