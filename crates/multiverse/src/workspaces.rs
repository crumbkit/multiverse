//! The workspace operations: create, list, find and remove.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::backend::Backend;
use crate::error::Error;
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
    /// The number of commits that the checkout's `HEAD` holds and no
    /// branch holds, such as commits on a detached `HEAD`, which the
    /// removal loses.
    pub stray_commits: u32,
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
        /// The number of commits on the branch that are not on its base.
        commits: u32,
    },
    /// The branch is deleted with commits that are not on its base,
    /// because the caller allowed it.
    DeletedUnmerged {
        /// The number of commits on the branch that are not on its base.
        commits: u32,
    },
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
/// disposable when one of its components has one of these names, such as
/// `target/` or `web/node_modules/`. Every other ignored path, such as
/// `.env`, stops a removal unless the caller allows discarding changes.
pub const DISPOSABLE: &[&str] = &["target", "node_modules", "dist", "build", ".cache"];

/// Reports whether `path`, as `git status` prints an ignored path, is in
/// [`DISPOSABLE`].
fn disposable(path: &str) -> bool {
    path.split('/')
        .any(|component| DISPOSABLE.contains(&component))
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
    /// the checkout's name in front, such as `inbox: target/`.
    pub disposable: Vec<String>,
    /// The changes and other ignored paths that the removal discards
    /// because the caller allowed it, in the form of
    /// [`RemovalPlan::Dirty`] and [`RemovalPlan::IgnoredFiles`].
    pub discarded: Vec<String>,
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
    /// A checkout holds ignored files outside [`DISPOSABLE`], such as
    /// `.env`, and the caller did not allow discarding changes.
    IgnoredFiles {
        /// The workspace's record.
        workspace: Workspace,
        /// Each ignored path, with the checkout's name in front, such as
        /// `inbox: .env`.
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
enum Entry {
    /// Nothing is at the path.
    Absent,
    /// A symbolic link is at the path.
    Link,
    /// A file or a directory is at the path.
    Present,
}

impl Entry {
    /// Returns what is at `path`. An entry that cannot be read counts as
    /// present, so a later step reports why.
    fn at(path: &Path) -> Self {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => Self::Link,
            Ok(_) => Self::Present,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self::Absent,
            Err(_) => Self::Present,
        }
    }
}

/// What a run of [`Workspaces::make`] created, so that
/// [`Workspaces::undo`] deletes that and nothing else.
#[derive(Debug, Default)]
struct Made {
    /// Whether the run created the workspace's directory.
    directory: bool,
    /// The number of checkouts that the run started to add. A checkout
    /// whose `worktree add` failed counts, because git may have left its
    /// directory and branch.
    checkouts: usize,
    /// Whether the run wrote `AGENTS.md`.
    agents: bool,
}

impl<B: Backend> Workspaces<B> {
    /// Returns the workspaces with their records in `store` and their
    /// checkouts made by `backend`.
    #[must_use]
    pub const fn new(store: Store, backend: B) -> Self {
        Self { store, backend }
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
        if Entry::at(&path) != Entry::Absent {
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
        let mut made = Made::default();
        match self.make(root, &mut workspace, &mut made) {
            Ok(()) => Ok(NewOutcome::Created(workspace)),
            Err(error) => {
                self.undo(&workspace, &made);
                Err(error)
            }
        }
    }

    /// Makes the directory, the checkouts and the `AGENTS.md` of
    /// `workspace`, whose record the store already holds, and writes the
    /// record again with each checkout's commit. It notes in `made` each
    /// thing it creates.
    fn make(&self, root: &Path, workspace: &mut Workspace, made: &mut Made) -> Result<(), Error> {
        let unwritable = |path: &Path| {
            let path = path.to_path_buf();
            move |source| Error::Unwritable { path, source }
        };
        fs::create_dir_all(root).map_err(unwritable(root))?;
        fs::create_dir(&workspace.path).map_err(unwritable(&workspace.path))?;
        made.directory = true;
        for checkout in &mut workspace.checkouts {
            made.checkouts += 1;
            let commit = self.backend.add_checkout(
                &checkout.source,
                &checkout.path,
                &checkout.branch,
                &checkout.base,
            )?;
            checkout.commit = Some(commit);
        }
        made.agents = true;
        store::replace_file(&workspace.agents_file(), agents_text(workspace).as_bytes())?;
        self.store.update(workspace)
    }

    /// Deletes what a failed [`Workspaces::make`] created, as `made`
    /// records it: each checkout it started, even one whose `worktree add`
    /// failed partway, each branch it created, its `AGENTS.md` and its
    /// directory. [`Workspaces::create`] checked that the branch and the
    /// directory did not exist, so a branch with the workspace's name was
    /// created by this run. Each step is best effort, because the caller
    /// reports the failure that stopped the creation. The record is deleted
    /// only when every step succeeded, so `mxv remove` can finish a cleanup
    /// that failed.
    fn undo(&self, workspace: &Workspace, made: &Made) {
        let mut clean = true;
        for checkout in workspace.checkouts.iter().take(made.checkouts) {
            if Entry::at(&checkout.path) != Entry::Absent {
                clean &= self
                    .backend
                    .remove_checkout(&checkout.source, &checkout.path, true)
                    .is_ok();
            }
            clean &= self.backend.prune_checkouts(&checkout.source).is_ok();
            if self
                .backend
                .has_branch(&checkout.source, &checkout.branch)
                .unwrap_or(true)
            {
                clean &= self
                    .backend
                    .delete_branch(&checkout.source, &checkout.branch)
                    .is_ok();
            }
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
        let directory = match Entry::at(&workspace.path) {
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
            let (directory, stray_commits) = match Entry::at(&checkout.path) {
                Entry::Absent => (Fate::Missing, 0),
                Entry::Link => {
                    return Ok(RemovalPlan::Unowned {
                        checkout: checkout.path.clone(),
                        workspace,
                    });
                }
                Entry::Present => {
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
                    let found = self.backend.changes(&checkout.path)?;
                    let named = |item: &String| format!("{}: {item}", checkout.name);
                    changes.extend(found.changed.iter().map(named));
                    for path in &found.ignored {
                        if disposable(path) {
                            disposables.push(named(path));
                        } else {
                            ignored.push(named(path));
                        }
                    }
                    let stray = self.backend.stray_commits(&checkout.path)?;
                    if stray > 0 && !allow.delete_unmerged {
                        return Ok(RemovalPlan::StrayCommits {
                            checkout: checkout.path.clone(),
                            commits: stray,
                            workspace,
                        });
                    }
                    (Fate::Removed, stray)
                }
            };
            let branch = if !source
                || !self
                    .backend
                    .has_branch(&checkout.source, &checkout.branch)?
            {
                BranchFate::Missing
            } else {
                let revision = format!("refs/heads/{}", checkout.branch);
                match self
                    .backend
                    .unmerged_commits(&checkout.source, &revision, &checkout.base)?
                {
                    0 => BranchFate::Deleted,
                    commits if allow.delete_unmerged => BranchFate::DeletedUnmerged { commits },
                    commits => BranchFate::KeptUnmerged { commits },
                }
            };
            checkouts.push(CheckoutRemoval {
                checkout: checkout.clone(),
                directory,
                branch,
                stray_commits,
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
                        removal.allow.discard_changes,
                    )?;
                }
                Fate::Missing | Fate::Kept => {
                    if checkout.source.is_dir() {
                        self.backend.prune_checkouts(&checkout.source)?;
                    }
                }
            }
            if matches!(
                planned.branch,
                BranchFate::Deleted | BranchFate::DeletedUnmerged { .. }
            ) {
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
