//! Rebuilding an archived workspace at its archived commits, and deleting
//! an archive that nobody wants back.

use std::path::{Path, PathBuf};

use crate::archive::{Archive, ArchiveState, ArchivedCheckout, Archives, root};
use crate::backend::Backend;
use crate::error::Error;
use crate::name::WorkspaceName;
use crate::record::{Checkout, Workspace, WorkspaceKind};
use crate::runs::Runs;
use crate::store::Created;
use crate::workspaces::{
    CloneStart, Entry, Made, Mode, Workspaces, default_branch, delete_directory,
};

/// Why `mxv restore` built nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RestoreRefusal {
    /// No archive has the name.
    NotFound,
    /// The archive has not finished, and `mxv archive` finishes it.
    Unfinished(Box<Archive>),
    /// A workspace holds the name, or something exists at the recorded
    /// path.
    NameTaken {
        /// The workspace's recorded directory.
        path: PathBuf,
        /// Whether a workspace record holds the name.
        record: bool,
    },
    /// A worktree's branch points at another commit than the pin.
    BranchMoved {
        /// The checkout's folder.
        folder: String,
        /// The branch.
        branch: String,
        /// The commit that the archive pinned.
        pinned: String,
        /// The commit that the branch points at now.
        tip: String,
    },
    /// The pinned commit is nowhere that mxv can read it.
    CommitMissing {
        /// The checkout's folder.
        folder: String,
        /// The commit.
        commit: String,
    },
    /// A file of the archive differs from its recorded digest.
    RootFileChanged {
        /// The file, relative to the workspace's directory.
        path: String,
    },
}

/// A restore that [`Workspaces::plan_restore`] found nothing in the way of.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct RestorePlan {
    /// The archive.
    pub archive: Archive,
    /// The folder of each clone whose mirror is gone, so the restore clones
    /// the mirror again from the recorded origin.
    pub missing_mirrors: Vec<String>,
}

/// The outcome of [`Workspaces::plan_restore`].
#[derive(Debug)]
pub enum RestorePlanned {
    /// Nothing is in the way.
    Ready(Box<RestorePlan>),
    /// The restore cannot go ahead.
    Refused(RestoreRefusal),
}

/// A host port that a restored workspace could not keep.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PortChange {
    /// The port's name in the manifest.
    pub name: String,
    /// The port that the workspace held.
    pub from: u16,
    /// The port that it holds now.
    pub to: u16,
}

impl PortChange {
    /// Returns the change of the port `name` from `from` to `to`.
    #[must_use]
    pub const fn new(name: String, from: u16, to: u16) -> Self {
        Self { name, from, to }
    }
}

/// What [`Workspaces::restore`] rebuilt.
#[derive(Debug)]
#[non_exhaustive]
pub struct Restored {
    /// The workspace's record.
    pub workspace: Workspace,
    /// The ports that changed.
    pub ports: Vec<PortChange>,
    /// What the restore could not finish after it wrote the record, such
    /// as a pin that it could not delete.
    pub warnings: Vec<String>,
}

/// The outcome of [`Workspaces::restore`].
#[derive(Debug)]
pub enum RestoreOutcome {
    /// The workspace is back.
    Restored(Box<Restored>),
    /// Nothing was built, and the archive is whole.
    Refused(RestoreRefusal),
}

/// What [`Workspaces::remove_archive`] did.
#[derive(Debug)]
#[non_exhaustive]
pub struct ArchivedRemoval {
    /// The archive that it deleted.
    pub archive: Archive,
    /// Why a pin could not be deleted, for each one.
    pub warnings: Vec<String>,
}

impl<B: Backend> Workspaces<B> {
    /// Returns whether the archive `name` can be restored, and changes
    /// nothing.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Archives::read`], and of the backend's
    /// lookups.
    pub fn plan_restore(
        &self,
        archives: &Archives,
        name: &WorkspaceName,
    ) -> Result<RestorePlanned, Error> {
        let refused = |refusal| Ok(RestorePlanned::Refused(refusal));
        let Some(archive) = archives.read(name)? else {
            return refused(RestoreRefusal::NotFound);
        };
        if archive.state != ArchiveState::Archived {
            return refused(RestoreRefusal::Unfinished(Box::new(archive)));
        }
        let record = self.store.read(name)?.is_some();
        if record || Entry::at(&archive.path)? != Entry::Absent {
            return refused(RestoreRefusal::NameTaken {
                path: archive.path,
                record,
            });
        }
        if let Some(path) = root::first_changed(archives, &archive)? {
            return refused(RestoreRefusal::RootFileChanged { path });
        }
        let mut missing_mirrors = Vec::new();
        for checkout in &archive.checkouts {
            if let Some(mirror) = &checkout.mirror {
                if !mirror.join("HEAD").is_file() {
                    missing_mirrors.push(checkout.folder.clone());
                    continue;
                }
                if self
                    .backend
                    .resolve_commit(mirror, &checkout.commit)?
                    .is_none()
                {
                    return refused(missing_commit(checkout));
                }
                continue;
            }
            if !checkout.source.is_dir() {
                return Err(Error::RepositoryNotFound {
                    path: checkout.source.clone(),
                });
            }
            if self
                .backend
                .resolve_commit(&checkout.source, &checkout.commit)?
                .is_none()
            {
                return refused(missing_commit(checkout));
            }
            if checkout.detached {
                continue;
            }
            let tip = self
                .backend
                .branch_tip(&checkout.source, &checkout.branch)?;
            if let Some(tip) = tip
                && tip != checkout.commit
            {
                return refused(RestoreRefusal::BranchMoved {
                    folder: checkout.folder.clone(),
                    branch: checkout.branch.clone(),
                    pinned: checkout.commit.clone(),
                    tip,
                });
            }
        }
        Ok(RestorePlanned::Ready(Box::new(RestorePlan {
            archive,
            missing_mirrors,
        })))
    }

    /// Rebuilds the workspace of `confirmed`, which
    /// [`Workspaces::plan_restore`] returned and the caller confirmed, and
    /// deletes the archive. It checks the plan again first. A failure
    /// deletes what the run made and keeps the archive.
    ///
    /// # Errors
    ///
    /// Returns the errors of the backend's commands and of the writes.
    pub fn restore(
        &self,
        archives: &Archives,
        confirmed: RestorePlan,
        runs: &Runs,
    ) -> Result<RestoreOutcome, Error> {
        let name = confirmed.archive.name.clone();
        let plan = match self.plan_restore(archives, &name)? {
            RestorePlanned::Ready(plan) => *plan,
            RestorePlanned::Refused(refusal) => return Ok(RestoreOutcome::Refused(refusal)),
        };
        let archive = plan.archive;
        let mut warnings = Vec::new();
        for checkout in &archive.checkouts {
            if let Some(mirror) = &checkout.mirror
                && let Some(refusal) = self.ensure_mirror(checkout, mirror, &mut warnings)?
            {
                return Ok(RestoreOutcome::Refused(refusal));
            }
        }
        (self.checkpoint)("mirrors")?;
        let mut workspace = archive.workspace();
        let mut made = Made {
            record_not_ours: true,
            ..Made::default()
        };
        let built = self.rebuild(archives, &archive, &mut workspace, &mut made, runs);
        let ports = match built {
            Ok(Some(ports)) => ports,
            Ok(None) => {
                self.unmake(&archive, &workspace, &made);
                return Ok(RestoreOutcome::Refused(RestoreRefusal::NameTaken {
                    path: archive.path.clone(),
                    record: true,
                }));
            }
            Err(error) => {
                self.unmake(&archive, &workspace, &made);
                return Err(error);
            }
        };
        (self.checkpoint)("record")?;
        if archive.containers_ran {
            runs.remember_containers(&archive.name)?;
        }
        warnings.extend(self.delete_pins(&archive));
        if let Err(error) = archives.delete(&archive.name) {
            warnings.push(format!(
                "mxv rebuilt the workspace and could not delete its archive; `mxv remove --archived {}` deletes it: {error}",
                archive.name
            ));
        }
        Ok(RestoreOutcome::Restored(Box::new(Restored {
            workspace,
            ports,
            warnings,
        })))
    }

    /// Deletes what a failed restore made, as `made` records it, and the
    /// root files that it wrote back.
    fn unmake(&self, archive: &Archive, workspace: &Workspace, made: &Made) {
        if made.directory {
            let _ = root::delete_entries(&archive.root, &archive.path);
        }
        self.undo(workspace, made);
    }

    /// Makes sure that the mirror of the clone `checkout` holds its commit.
    /// A mirror that has gone is cloned again from the recorded origin, and
    /// a failed fetch deletes the empty mirror that the run made. The mirror
    /// that is still there is fetched when the origin answers, because its
    /// branches are what the clone starts with, and the pin keeps the commit
    /// when it does not. A clone without an origin depends on its pin.
    fn ensure_mirror(
        &self,
        checkout: &ArchivedCheckout,
        mirror: &Path,
        warnings: &mut Vec<String>,
    ) -> Result<Option<RestoreRefusal>, Error> {
        let _lock = self.lock_mirror(mirror)?;
        let exists = mirror.join("HEAD").is_file();
        if !exists && checkout.origin.is_none() {
            return Ok(Some(missing_commit(checkout)));
        }
        if let Some(origin) = &checkout.origin {
            self.backend.prepare_mirror(mirror)?;
            let fetched =
                self.backend
                    .fetch_mirror(mirror, origin, &default_branch(&checkout.base));
            match fetched {
                Ok(_) => {}
                Err(error) if exists => warnings.push(format!(
                    "mxv could not fetch the origin of {}, so the restored clone starts from what its mirror last fetched: {error}",
                    checkout.folder
                )),
                Err(error) => {
                    delete_directory(mirror)?;
                    return Err(error);
                }
            }
        }
        if self
            .backend
            .resolve_commit(mirror, &checkout.commit)?
            .is_none()
        {
            return Ok(Some(missing_commit(checkout)));
        }
        Ok(None)
    }

    /// Makes the workspace's directory and checkouts, writes the root files
    /// and the workspace's record, and returns the ports that changed, or
    /// `None` when another record took the name first.
    fn rebuild(
        &self,
        archives: &Archives,
        archive: &Archive,
        workspace: &mut Workspace,
        made: &mut Made,
        runs: &Runs,
    ) -> Result<Option<Vec<PortChange>>, Error> {
        let unwritable = |path: &Path| {
            let path = path.to_path_buf();
            move |source| Error::Unwritable { path, source }
        };
        if let Some(parent) = archive.path.parent() {
            std::fs::create_dir_all(parent).map_err(unwritable(parent))?;
        }
        std::fs::create_dir(&archive.path).map_err(unwritable(&archive.path))?;
        made.directory = true;
        (self.checkpoint)("directory")?;
        for (checkout, archived) in workspace.checkouts.iter_mut().zip(&archive.checkouts) {
            if archive.kind == WorkspaceKind::Clone {
                self.rebuild_clone(checkout, archived, made)?;
            } else {
                self.rebuild_worktree(checkout, archived, made)?;
            }
            (self.checkpoint)(&format!("checkout:{}", checkout.name))?;
        }
        made.agents = true;
        self.write_agents(workspace)?;
        made.manifest = archive.manifest.is_some();
        root::write_back(archives, archive, &archive.path, &workspace.manifest_file())?;
        (self.checkpoint)("root")?;
        runs.restored_ports(workspace, &self.store, |workspace, store| {
            Ok(store.create(workspace)? == Created::Written)
        })
    }

    /// Makes the clone of `checkout` at its archived commit, through the
    /// code that `mxv new` makes a clone with.
    fn rebuild_clone(
        &self,
        checkout: &mut Checkout,
        archived: &ArchivedCheckout,
        made: &mut Made,
    ) -> Result<(), Error> {
        // A clone starts with its mirror's path as a placeholder origin
        // when the archived clone had none, and loses it below.
        let origin = archived
            .origin
            .clone()
            .unwrap_or_else(|| archived.pin_repository().display().to_string());
        let start = if archived.detached {
            CloneStart::Detached(&archived.commit)
        } else {
            CloneStart::Pinned(&archived.commit)
        };
        self.make_clone_at(checkout, &origin, start, false, &mut made.clones)?;
        if archived.origin.is_none() {
            self.backend.remove_origin(&checkout.path)?;
        }
        if let Some(tracked) = archived.upstream.as_ref().filter(|_| !archived.detached) {
            let remote = archived.upstream_remote.as_deref().unwrap_or("origin");
            self.backend
                .set_upstream(&checkout.path, &checkout.branch, remote, tracked)?;
        }
        Ok(())
    }

    /// Makes the worktree of `checkout` at its archived commit, on its
    /// branch, which the archive left in the repository, or which the run
    /// creates at the pin when it has gone.
    fn rebuild_worktree(
        &self,
        checkout: &mut Checkout,
        archived: &ArchivedCheckout,
        made: &mut Made,
    ) -> Result<(), Error> {
        // A restore that stopped partway leaves the repository listing a
        // worktree whose directory the person deleted, and git refuses to
        // check the branch out again until that entry is gone.
        self.backend.prune_checkouts(&checkout.source)?;
        if archived.detached {
            made.branches.push(None);
            let commit = self.backend.add_detached_checkout(
                &checkout.source,
                &checkout.path,
                &archived.commit,
            )?;
            checkout.commit = Some(commit);
            return Ok(());
        }
        let created = if self
            .backend
            .branch_tip(&checkout.source, &checkout.branch)?
            .is_some()
        {
            None
        } else {
            self.backend
                .create_branch(&checkout.source, &checkout.branch, &archived.commit)?
        };
        made.branches.push(created);
        let commit =
            self.backend
                .add_checkout(&checkout.source, &checkout.path, &checkout.branch)?;
        checkout.commit = Some(commit);
        Ok(())
    }

    /// Deletes the pins of `archive`, and returns why one could not be
    /// deleted, for each one.
    fn delete_pins(&self, archive: &Archive) -> Vec<String> {
        let mut warnings = Vec::new();
        for checkout in &archive.checkouts {
            let repository = checkout.pin_repository();
            if !repository.is_dir() {
                continue;
            }
            if let Err(error) = self.backend.delete_ref(repository, &checkout.pin) {
                warnings.push(format!(
                    "mxv could not delete the pin {} in {}: {error}",
                    checkout.pin,
                    repository.display()
                ));
            }
        }
        warnings
    }

    /// Deletes the archive `name` and its pins, and returns what it deleted,
    /// or `None` when no archive has the name. The caller reports an archive
    /// that has not finished.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unwritable`] when the archive cannot be deleted.
    pub fn remove_archive(
        &self,
        archives: &Archives,
        archive: Archive,
        mode: Mode,
    ) -> Result<ArchivedRemoval, Error> {
        if mode == Mode::DryRun {
            return Ok(ArchivedRemoval {
                archive,
                warnings: Vec::new(),
            });
        }
        let warnings = self.delete_pins(&archive);
        archives.delete(&archive.name)?;
        Ok(ArchivedRemoval { archive, warnings })
    }
}

/// Returns the refusal of a checkout whose commit is gone.
fn missing_commit(checkout: &ArchivedCheckout) -> RestoreRefusal {
    RestoreRefusal::CommitMissing {
        folder: checkout.folder.clone(),
        commit: checkout.commit.clone(),
    }
}
