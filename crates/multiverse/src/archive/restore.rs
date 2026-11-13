//! Rebuilding an archived workspace at its archived commits with the git
//! state that the archive kept, and deleting an archive that nobody wants
//! back.

use std::path::{Path, PathBuf};

use crate::archive::ignored::{self, IgnoredRefusal};
use crate::archive::{
    Archive, ArchiveState, ArchivedCheckout, Archives, KEPT_NAMESPACE, Kept, root,
};
use crate::backend::{Backend, RefHolder, WipCommits};
use crate::error::Error;
use crate::health::{self, Rebuilt};
use crate::name::WorkspaceName;
use crate::record::{Checkout, Workspace, WorkspaceKind};
use crate::runs::Runs;
use crate::store::Created;
use crate::workspaces::{
    CloneStart, Entry, Made, Mode, Workspaces, default_branch, delete_directory,
};

/// What a restore may do beyond rebuilding from local state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RestoreOptions {
    /// Whether the restore goes ahead without an ignored entry that is
    /// missing from the tar or differs from its digest, and lists it as
    /// lost.
    pub force: bool,
    /// Whether the restore may open a network connection to each clone's
    /// origin. Without it, a restore reads only local state and refuses
    /// when a clone's mirror is gone. With it, a mirror that is gone is
    /// made again from the origin, and the restore keeps only what the
    /// origin holds.
    pub from_origin: bool,
}

impl RestoreOptions {
    /// Returns the options with `force` and `from_origin` as given.
    #[must_use]
    pub const fn new(force: bool, from_origin: bool) -> Self {
        Self { force, from_origin }
    }
}

/// What a clone's lost mirror took with it, for one checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MirrorLoss {
    /// The checkout's folder.
    pub folder: String,
    /// The mirror that is gone.
    pub mirror: PathBuf,
    /// What the archive kept in that mirror besides `HEAD`, one phrase for
    /// each kind, such as `1 stash`. The commit of `HEAD` is not listed,
    /// because the origin may hold it.
    pub items: Vec<String>,
}

/// Something a restore from origin could not put back, because only the
/// lost mirror held it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LostPin {
    /// The checkout's folder.
    pub folder: String,
    /// What was lost, in a phrase.
    pub what: String,
}

/// Why `mxv restore` built nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RestoreRefusal {
    /// A clone's mirror is gone, and the restore may not ask the origin.
    /// `--from-origin` makes the mirror again and restores what the origin
    /// holds.
    MirrorMissing {
        /// Each checkout whose mirror is gone, with what it would lose.
        checkouts: Vec<MirrorLoss>,
    },
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
    /// An entry of the archive's tar of ignored files is missing or differs
    /// from its digest; `--force` restores without it.
    IgnoredDamaged {
        /// Each archived path, as `<folder>/<path>`.
        paths: Vec<String>,
    },
    /// The rebuilt workspace already holds something at an ignored entry's
    /// path.
    IgnoredTaken {
        /// Each archived path, as `<folder>/<path>`.
        paths: Vec<String>,
    },
}

/// A restore that [`Workspaces::plan_restore`] found nothing in the way of.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct RestorePlan {
    /// The archive.
    pub archive: Archive,
    /// The folder of each clone whose mirror is gone, so the restore, which
    /// the caller asked to fetch, clones the mirror again from the recorded
    /// origin.
    pub missing_mirrors: Vec<String>,
    /// Whether the restore goes ahead without an ignored entry that is
    /// missing from the tar or differs from its digest, and lists it as
    /// lost.
    pub force: bool,
    /// Whether the restore fetches each missing mirror from its origin.
    pub from_origin: bool,
}

/// The outcome of [`Workspaces::plan_restore`].
#[derive(Debug)]
pub enum RestorePlanned {
    /// Nothing is in the way.
    Ready(Box<RestorePlan>),
    /// A workspace record holds the name and no archive needs restoring,
    /// so there is nothing to do. `stale` is the archive that an earlier
    /// restore rebuilt and stopped before it deleted, which has the
    /// workspace's stable id while the workspace's directory and every
    /// checkout's directory are there, as [`Rebuilt::Whole`] says;
    /// [`Workspaces::remove_archive`] deletes it.
    Unchanged {
        /// The archive that an earlier restore left, or `None`.
        stale: Option<Box<Archive>>,
    },
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

/// The git state besides `HEAD` that a restore put back into one checkout.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PutBack {
    /// The checkout's folder.
    pub folder: String,
    /// The number of files with staged changes.
    pub staged: usize,
    /// The number of files with unstaged changes.
    pub unstaged: usize,
    /// The number of untracked paths.
    pub untracked: usize,
    /// The number of stash entries.
    pub stashes: usize,
    /// Each local branch besides the checkout's own.
    pub branches: Vec<String>,
    /// Each tag that `origin` does not hold.
    pub tags: Vec<String>,
    /// Each per-worktree reference, by its name below `refs/worktree/`.
    pub worktree: Vec<String>,
    /// Each reference below [`KEPT_NAMESPACE`] that keeps a commit no other
    /// reference reaches.
    pub kept: Vec<String>,
    /// Each submodule that the checkout's index names, which the restore
    /// leaves uninitialised.
    pub submodules: Vec<String>,
    /// Each ignored file or link written back into the checkout from the
    /// archive's tar, as its archived path, such as `app/.env`.
    pub ignored: Vec<String>,
}

impl PutBack {
    /// Reports whether the restore put back nothing besides `HEAD` and
    /// found no submodule.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self
            == Self {
                folder: self.folder.clone(),
                ..Self::default()
            }
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
    /// What the restore put back into each checkout, in the record's
    /// order.
    pub put_back: Vec<PutBack>,
    /// Each ignored entry, as its archived path, that a forced restore left
    /// out because the tar lacked it or it differed from its digest.
    pub lost: Vec<String>,
    /// Each pin of a lost mirror that origin did not hold, so the restore
    /// could not put it back.
    pub lost_pins: Vec<LostPin>,
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
    /// nothing. With `options.force`, the restore leaves out an ignored
    /// entry whose tar entry is damaged instead of refusing. It reads only
    /// local state: a clone whose mirror is gone refuses with
    /// [`RestoreRefusal::MirrorMissing`] unless `options.from_origin`.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Archives::read`], and of the backend's
    /// lookups.
    pub fn plan_restore(
        &self,
        archives: &Archives,
        name: &WorkspaceName,
        options: RestoreOptions,
    ) -> Result<RestorePlanned, Error> {
        let RestoreOptions { force, from_origin } = options;
        let refused = |refusal| Ok(RestorePlanned::Refused(refusal));
        let workspace = self.store.read(name)?;
        let Some(archive) = archives.read(name)? else {
            if workspace.is_some() {
                return Ok(RestorePlanned::Unchanged { stale: None });
            }
            return refused(RestoreRefusal::NotFound);
        };
        if archive.state != ArchiveState::Archived {
            return refused(RestoreRefusal::Unfinished(Box::new(archive)));
        }
        // A workspace of the archive's id whose directories are partly
        // gone refuses below, because the archive may hold the only copies
        // of its files.
        if let Some(workspace) = &workspace
            && health::rebuilt(&archive, workspace) == Rebuilt::Whole
        {
            return Ok(RestorePlanned::Unchanged {
                stale: Some(Box::new(archive)),
            });
        }
        let record = workspace.is_some();
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
        let mut gone = Vec::new();
        for checkout in &archive.checkouts {
            if let Some(mirror) = &checkout.mirror {
                let exists = mirror.join("HEAD").is_file();
                if !exists && checkout.origin.is_none() {
                    return refused(missing_commit(checkout));
                }
                if !exists || (from_origin && self.pins_gone(checkout)?) {
                    if from_origin {
                        missing_mirrors.push(checkout.folder.clone());
                    } else {
                        gone.push(MirrorLoss {
                            folder: checkout.folder.clone(),
                            mirror: mirror.clone(),
                            items: lost_items(&checkout.kept),
                        });
                    }
                    continue;
                }
                if let Some(refusal) = self.missing(checkout)? {
                    return refused(refusal);
                }
                continue;
            }
            if !checkout.source.is_dir() {
                return Err(Error::RepositoryNotFound {
                    path: checkout.source.clone(),
                });
            }
            if let Some(refusal) = self.missing(checkout)? {
                return refused(refusal);
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
        if !gone.is_empty() {
            return refused(RestoreRefusal::MirrorMissing { checkouts: gone });
        }
        Ok(RestorePlanned::Ready(Box::new(RestorePlan {
            archive,
            missing_mirrors,
            force,
            from_origin,
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
        let options = RestoreOptions::new(confirmed.force, confirmed.from_origin);
        let plan = match self.plan_restore(archives, &name, options)? {
            RestorePlanned::Ready(plan) => *plan,
            RestorePlanned::Refused(refusal) => return Ok(RestoreOutcome::Refused(refusal)),
            // Another command restored the workspace after the plan.
            RestorePlanned::Unchanged { .. } => {
                return Ok(RestoreOutcome::Refused(RestoreRefusal::NameTaken {
                    path: confirmed.archive.path,
                    record: true,
                }));
            }
        };
        let original = plan.archive;
        let mut archive = original.clone();
        let mut warnings = Vec::new();
        let mut lost_pins = Vec::new();
        let mut fresh = Vec::new();
        if plan.from_origin {
            for (checkout, kept) in archive.checkouts.iter_mut().zip(&original.checkouts) {
                let Some(mirror) = kept.mirror.as_deref() else {
                    continue;
                };
                if self.ensure_mirror(kept, mirror, &mut warnings)? {
                    if let Some(refusal) = self.salvage(checkout, &mut lost_pins)? {
                        return Ok(RestoreOutcome::Refused(refusal));
                    }
                    fresh.push(kept.folder.clone());
                } else if let Some(refusal) = self.missing(kept)? {
                    return Ok(RestoreOutcome::Refused(refusal));
                }
            }
        }
        (self.checkpoint)("mirrors")?;
        let mut workspace = archive.workspace();
        let mut made = Made {
            record_not_ours: true,
            ..Made::default()
        };
        let mut put_back = Vec::new();
        let mut lost = Vec::new();
        let built = self.rebuild(
            archives,
            &archive,
            plan.force,
            &mut workspace,
            &mut made,
            &mut put_back,
            &mut lost,
            runs,
        );
        let ports = match built {
            Ok(Ok(ports)) => ports,
            Ok(Err(refusal)) => {
                self.unmake(&archive, &workspace, &made);
                return Ok(RestoreOutcome::Refused(refusal));
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
        // A mirror made again holds none of the archive's pins.
        let mut pinned = original;
        pinned
            .checkouts
            .retain(|checkout| !fresh.contains(&checkout.folder));
        warnings.extend(self.delete_pins(&pinned));
        if let Err(error) = archives.delete(&archive) {
            warnings.push(format!(
                "mxv rebuilt the workspace and could not delete its archive; `mxv remove --archived {}` deletes it: {error}",
                archive.name
            ));
        }
        Ok(RestoreOutcome::Restored(Box::new(Restored {
            workspace,
            ports,
            put_back,
            lost,
            lost_pins,
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

    /// Fetches the origin of the clone `checkout` into its mirror, as a
    /// person asked with `--from-origin`, and returns whether the mirror
    /// holds none of the archive's pins, so the caller restores only what
    /// the origin holds. A mirror that is gone is made again, and a failed
    /// fetch deletes the empty mirror that the run made. A mirror that
    /// still holds the archive's pins is fetched when the origin answers,
    /// because its branches are what the clone starts with, and the pin
    /// keeps the commit when it does not.
    fn ensure_mirror(
        &self,
        checkout: &ArchivedCheckout,
        mirror: &Path,
        warnings: &mut Vec<String>,
    ) -> Result<bool, Error> {
        let _lock = self.lock_mirror(mirror)?;
        let exists = mirror.join("HEAD").is_file();
        let fresh = !exists || self.pins_gone(checkout)?;
        let Some(origin) = &checkout.origin else {
            return Ok(false);
        };
        self.backend.prepare_mirror(mirror)?;
        let fetched = self
            .backend
            .fetch_mirror(mirror, origin, &default_branch(&checkout.base));
        match fetched {
            Ok(()) => {}
            Err(error) if !fresh => warnings.push(format!(
                "mxv could not fetch the origin of {}, so the restored clone starts from what its mirror last fetched: {error}",
                checkout.folder
            )),
            Err(error) => {
                if !exists {
                    delete_directory(mirror)?;
                }
                return Err(error);
            }
        }
        Ok(fresh)
    }

    /// Reports whether the mirror of `checkout` no longer holds the pin of
    /// its `HEAD`, so every pin that the archive kept there is gone.
    fn pins_gone(&self, checkout: &ArchivedCheckout) -> Result<bool, Error> {
        Ok(self
            .backend
            .resolve_commit(checkout.pin_holder(), &checkout.pin)?
            .is_none())
    }

    /// Changes `checkout`, whose mirror was made again from the origin, to
    /// what that mirror holds: it starts at the archived commit when the
    /// origin holds it, else at the tip of the archived branch on the
    /// origin, else at the origin's default branch, and it keeps only the
    /// pins whose objects the mirror holds. It adds each thing it leaves
    /// out to `lost`, and returns the refusal when the origin holds no
    /// branch to start from.
    fn salvage(
        &self,
        checkout: &mut ArchivedCheckout,
        lost: &mut Vec<LostPin>,
    ) -> Result<Option<RestoreRefusal>, Error> {
        let Some(mirror) = checkout.mirror.clone() else {
            return Ok(None);
        };
        let holder = RefHolder::Mirror(&mirror);
        let holds = |object: &str| -> Result<bool, Error> {
            Ok(self.backend.resolve_commit(holder, object)?.is_some())
        };
        let folder = checkout.folder.clone();
        let mut lose = |what: String| {
            lost.push(LostPin {
                folder: folder.clone(),
                what,
            });
        };
        if !holds(&checkout.commit)? {
            let archived = checkout.commit.clone();
            let default = default_branch(&checkout.base);
            let mut start = None;
            for branch in [checkout.branch.as_str(), default.as_str()] {
                if let Some(tip) = self
                    .backend
                    .resolve_commit(holder, &format!("refs/heads/{branch}"))?
                {
                    start = Some((branch.to_owned(), tip));
                    break;
                }
            }
            let Some((branch, tip)) = start else {
                return Ok(Some(missing_commit(checkout)));
            };
            lose(format!(
                "the archived commit {archived}, so the checkout starts at {tip}, the tip of {branch} on origin"
            ));
            checkout.commit = tip;
            checkout.upstream = None;
        }
        let kept = std::mem::take(&mut checkout.kept);
        let mut now = Kept::default();
        if let Some(wip) = kept.wip {
            if holds(&wip.index)? && holds(&wip.worktree)? {
                now.wip = Some(wip);
            } else {
                lose("the uncommitted work".to_owned());
            }
        }
        for stash in kept.stashes {
            if holds(&stash.commit)? {
                now.stashes.push(stash);
            } else {
                lose(format!("the stash {}", stash.message));
            }
        }
        let named = [
            ("the local branch", kept.branches, &mut now.branches),
            ("the tag", kept.tags, &mut now.tags),
            (
                "the per-worktree reference",
                kept.worktree,
                &mut now.worktree,
            ),
        ];
        for (label, names, keep) in named {
            for (name, object) in names {
                if holds(&object)? {
                    keep.insert(name, object);
                } else {
                    lose(format!("{label} {name}"));
                }
            }
        }
        for commit in kept.orphans {
            if holds(&commit)? {
                now.orphans.push(commit);
            } else {
                lose(format!("the commit {commit}, which no branch reaches"));
            }
        }
        checkout.kept = now;
        Ok(None)
    }

    /// Returns the refusal of `checkout` when the repository that holds its
    /// pins lacks its commit or a commit that the archive kept.
    fn missing(&self, checkout: &ArchivedCheckout) -> Result<Option<RestoreRefusal>, Error> {
        let mut commits = vec![checkout.commit.clone()];
        commits.extend(checkout.kept.commits());
        for commit in commits {
            if self
                .backend
                .resolve_commit(checkout.pin_holder(), &commit)?
                .is_none()
            {
                return Ok(Some(RestoreRefusal::CommitMissing {
                    folder: checkout.folder.clone(),
                    commit,
                }));
            }
        }
        Ok(None)
    }

    /// Makes the workspace's directory and checkouts, writes the ignored
    /// files back, puts back what the archive kept of each checkout into
    /// `put_back`, writes the root files and the workspace's record, and
    /// returns the ports that changed. It returns the refusal when an
    /// ignored entry is damaged, and `force` is not set, or its path is
    /// taken, and [`RestoreRefusal::NameTaken`] when another record took the
    /// name first. A forced restore adds each damaged entry to `lost`.
    #[expect(
        clippy::too_many_arguments,
        reason = "the rebuild fills the three lists that the outcome reports"
    )]
    fn rebuild(
        &self,
        archives: &Archives,
        archive: &Archive,
        force: bool,
        workspace: &mut Workspace,
        made: &mut Made,
        put_back: &mut Vec<PutBack>,
        lost: &mut Vec<String>,
        runs: &Runs,
    ) -> Result<Result<Vec<PortChange>, RestoreRefusal>, Error> {
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
        for (checkout, archived) in workspace.checkouts.iter().zip(&archive.checkouts) {
            if let Some(wip) = &archived.kept.wip {
                self.backend.apply_wip(
                    &checkout.path,
                    &WipCommits {
                        index: wip.index.clone(),
                        worktree: wip.worktree.clone(),
                    },
                )?;
            }
        }
        (self.checkpoint)("apply")?;
        // The ignored files come after the checkouts and the uncommitted
        // work, so a path that the rebuilt checkout holds is found and
        // refused rather than written over.
        let mut written = Vec::new();
        if !archive.ignored.is_empty() {
            let data = archives.data_of(&archive.id);
            let checkouts: Vec<(&str, &Path)> = workspace
                .checkouts
                .iter()
                .map(|checkout| (checkout.name.as_str(), checkout.path.as_path()))
                .collect();
            match ignored::restore(&data, &archive.ignored, &checkouts, force)? {
                Ok(restored) => {
                    written = restored.written;
                    lost.extend(restored.lost);
                }
                Err(IgnoredRefusal::Damaged(paths)) => {
                    return Ok(Err(RestoreRefusal::IgnoredDamaged { paths }));
                }
                Err(IgnoredRefusal::Taken(paths)) => {
                    return Ok(Err(RestoreRefusal::IgnoredTaken { paths }));
                }
            }
        }
        (self.checkpoint)("ignored")?;
        for (checkout, archived) in workspace.checkouts.iter().zip(&archive.checkouts) {
            let mut kept = self.put_back_kept(archive.kind, checkout, &archived.kept, made)?;
            kept.ignored = written
                .iter()
                .filter(|path| folder_of(path) == checkout.name)
                .cloned()
                .collect();
            put_back.push(kept);
        }
        (self.checkpoint)("kept")?;
        made.agents = true;
        self.write_agents(workspace)?;
        made.manifest = archive.manifest.is_some();
        root::write_back(archives, archive, &archive.path, &workspace.manifest_file())?;
        (self.checkpoint)("root")?;
        // The archive leaves the marker out, so the run writes it again,
        // after the root entries in case an older archive holds one.
        self.write_marker(workspace, made)?;
        let ports = runs.restored_ports(workspace, &self.store, |workspace, store| {
            Ok(store.create(workspace)? == Created::Written)
        })?;
        Ok(ports.ok_or_else(|| RestoreRefusal::NameTaken {
            path: archive.path.clone(),
            record: true,
        }))
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

    /// Puts back what `kept` holds into `checkout`, a checkout of a
    /// workspace of `kind` whose uncommitted work is back already, and
    /// returns what it put back. A clone's references go with the clone, so
    /// a reference that the clone already holds, such as a branch that the
    /// clone started with, takes the kept value. A reference that it makes
    /// in a worktree's shared repository is recorded in `made`, so a failed
    /// restore deletes it.
    fn put_back_kept(
        &self,
        kind: WorkspaceKind,
        checkout: &Checkout,
        kept: &Kept,
        made: &mut Made,
    ) -> Result<PutBack, Error> {
        let path = &checkout.path;
        let mut put_back = PutBack {
            folder: checkout.name.clone(),
            stashes: kept.stashes.len(),
            branches: kept.branches.keys().cloned().collect(),
            tags: kept.tags.keys().cloned().collect(),
            worktree: kept.worktree.keys().cloned().collect(),
            ..PutBack::default()
        };
        if kept.wip.is_some() {
            for change in self.backend.changes(path)?.changed {
                let mut marks = change.chars();
                let (staged, unstaged) = (marks.next(), marks.next());
                if staged == Some('?') {
                    put_back.untracked += 1;
                    continue;
                }
                put_back.staged += usize::from(staged.is_some_and(|mark| mark != ' '));
                put_back.unstaged += usize::from(unstaged.is_some_and(|mark| mark != ' '));
            }
        }
        // `stash store` makes each entry the newest, so the oldest goes
        // first and the stash list reads as it did.
        for stash in &kept.stashes {
            self.backend
                .store_stash(path, &stash.commit, &stash.message)?;
        }
        let named = [
            ("refs/heads/", &kept.branches),
            ("refs/tags/", &kept.tags),
            ("refs/worktree/", &kept.worktree),
        ];
        for (prefix, names) in named {
            for (name, object) in names {
                self.backend
                    .write_ref(path, &format!("{prefix}{name}"), object)?;
            }
        }
        for commit in &kept.orphans {
            let reference = format!("{KEPT_NAMESPACE}{commit}");
            let created = self.backend.create_ref(path, &reference, commit)?;
            if created && kind == WorkspaceKind::Worktree {
                made.refs.push((checkout.source.clone(), reference.clone()));
            }
            put_back.kept.push(reference);
        }
        put_back.submodules = self.backend.submodule_paths(path)?;
        Ok(put_back)
    }

    /// Deletes every pin of `archive`, those that its record names and any
    /// other below each checkout's pin directory, and returns why one could
    /// not be deleted, for each one.
    fn delete_pins(&self, archive: &Archive) -> Vec<String> {
        let mut warnings = Vec::new();
        for checkout in &archive.checkouts {
            let repository = checkout.pin_repository();
            if !repository.is_dir() {
                continue;
            }
            let holder = checkout.pin_holder();
            let dir = checkout.pin_dir();
            let mut pins: Vec<String> = checkout.pins().into_iter().map(|(pin, _)| pin).collect();
            match self.backend.refs_under(holder, dir) {
                Ok(found) => {
                    for (rest, _) in found {
                        let pin = format!("{dir}{rest}");
                        if !pins.contains(&pin) {
                            pins.push(pin);
                        }
                    }
                }
                Err(error) => warnings.push(format!(
                    "mxv could not list the pins below {dir} in {}: {error}",
                    repository.display()
                )),
            }
            for pin in pins {
                if let Err(error) = self.backend.delete_ref(holder, &pin) {
                    warnings.push(format!(
                        "mxv could not delete the pin {pin} in {}: {error}",
                        repository.display()
                    ));
                }
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
        archives.delete(&archive)?;
        Ok(ArchivedRemoval { archive, warnings })
    }
}

/// Returns the checkout's folder of the archived path `path`, such as `app`
/// of `app/.env`.
fn folder_of(path: &str) -> &str {
    path.split('/').next().unwrap_or(path)
}

/// Returns the refusal of a checkout whose commit is gone.
fn missing_commit(checkout: &ArchivedCheckout) -> RestoreRefusal {
    RestoreRefusal::CommitMissing {
        folder: checkout.folder.clone(),
        commit: checkout.commit.clone(),
    }
}

/// Returns one phrase for each kind of thing that `kept` holds, such as
/// `1 stash`, for a refusal that lists what a lost mirror took with it.
fn lost_items(kept: &Kept) -> Vec<String> {
    let plural = |count: usize, one: &str, many: &str| {
        format!("{count} {}", if count == 1 { one } else { many })
    };
    let mut items = Vec::new();
    if kept.wip.is_some() {
        items.push("uncommitted work".to_owned());
    }
    if !kept.stashes.is_empty() {
        items.push(plural(kept.stashes.len(), "stash", "stashes"));
    }
    for name in kept.branches.keys() {
        items.push(format!("the local branch {name}"));
    }
    for name in kept.tags.keys() {
        items.push(format!("the tag {name}"));
    }
    if !kept.worktree.is_empty() {
        items.push(plural(
            kept.worktree.len(),
            "per-worktree reference",
            "per-worktree references",
        ));
    }
    if !kept.orphans.is_empty() {
        items.push(format!(
            "{} that no branch reaches",
            plural(kept.orphans.len(), "commit", "commits")
        ));
    }
    items
}
