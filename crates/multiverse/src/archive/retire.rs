//! Carrying out an archive: pinning each commit, writing the record, copying
//! the root files, and retiring each checkout.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use crate::archive::admit::{ArchivePlan, ArchivePlanned, ArchiveRefusal, submodule_items};
use crate::archive::root;
use crate::archive::{Archive, ArchiveState, ArchivedCheckout, Archives, WIP_MESSAGE, quarantine};
use crate::backend::{Backend, Head};
use crate::error::Error;
use crate::name::WorkspaceName;
use crate::record::{Checkout, Workspace, WorkspaceKind};
use crate::store;
use crate::workspaces::{
    BranchFate, CheckoutRemoval, Entry, Fate, Mode, Removal, RemovalOutcome, Workspaces, aside,
    delete_directory,
};

/// The outcome of [`Workspaces::archive`].
#[derive(Debug)]
pub enum ArchiveOutcome {
    /// The workspace is gone, and the record still says
    /// [`ArchiveState::Retiring`] until [`Workspaces::finish_archive`] marks
    /// it.
    Retired {
        /// The record.
        archive: Box<Archive>,
        /// What the removal of the workspace did.
        removal: Box<Removal>,
    },
    /// The archive stopped, and the workspace's remaining checkouts are in
    /// their places.
    Refused(ArchiveRefusal),
}

/// A checkout's git state and path list at one moment.
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    /// What `HEAD` names.
    head: Head,
    /// Each uncommitted change and untracked path.
    changed: Vec<String>,
    /// Each ignored path.
    ignored: Vec<String>,
    /// The commit of each stash entry.
    stashes: Vec<String>,
    /// The commits that only this checkout holds.
    unpushed: Vec<String>,
    /// What `git status` does not show, one item for each.
    concealed: Vec<String>,
    /// Each path below the checkout, outside `.git` and the disposable
    /// directories.
    paths: BTreeSet<String>,
}

impl Snapshot {
    /// Returns what differs between this snapshot and `later`, one sentence
    /// for each difference.
    fn differences(&self, later: &Self) -> Vec<String> {
        let mut found = Vec::new();
        if self.head.commit != later.head.commit || self.head.branch != later.head.branch {
            found.push(format!(
                "HEAD moved from {:?} to {:?}",
                self.head.commit, later.head.commit
            ));
        }
        let lists = [
            ("change", &self.changed, &later.changed),
            ("ignored path", &self.ignored, &later.ignored),
            ("stash", &self.stashes, &later.stashes),
            ("commit", &self.unpushed, &later.unpushed),
            ("hidden work", &self.concealed, &later.concealed),
        ];
        for (what, before, after) in lists {
            found.extend(
                after
                    .iter()
                    .filter(|item| !before.contains(item))
                    .map(|item| format!("new {what}: {item}")),
            );
            found.extend(
                before
                    .iter()
                    .filter(|item| !after.contains(item))
                    .map(|item| format!("{what} is gone: {item}")),
            );
        }
        found.extend(
            later
                .paths
                .difference(&self.paths)
                .map(|path| format!("new path: {path}")),
        );
        found.extend(
            self.paths
                .difference(&later.paths)
                .map(|path| format!("path is gone: {path}")),
        );
        found
    }
}

/// Adds to `paths` each path below `dir`, relative to `root`, that neither
/// is `.git` at the top nor one of `skip`, without following a symbolic
/// link.
fn collect_paths(
    root: &Path,
    dir: &Path,
    skip: &BTreeSet<&str>,
    paths: &mut BTreeSet<String>,
) -> Result<(), Error> {
    let unreadable = |source| Error::StateUnreadable {
        path: dir.to_path_buf(),
        source,
    };
    for entry in fs::read_dir(dir).map_err(unreadable)? {
        let entry = entry.map_err(unreadable)?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        if relative == ".git" || skip.contains(relative.as_str()) {
            continue;
        }
        let is_dir = entry.file_type().map_err(unreadable)?.is_dir();
        paths.insert(relative);
        if is_dir {
            collect_paths(root, &path, skip, paths)?;
        }
    }
    Ok(())
}

impl<B: Backend> Workspaces<B> {
    /// Moves back each checkout of `workspace` that an earlier run left
    /// under the name that [`quarantine`] returns, so it is read in its
    /// place.
    pub(super) fn put_back(&self, workspace: &Workspace) -> Result<(), Error> {
        for checkout in &workspace.checkouts {
            let Some(held) = quarantine(&checkout.path) else {
                continue;
            };
            if Entry::at(&held)? == Entry::Present && Entry::at(&checkout.path)? == Entry::Absent {
                self.move_back(workspace, checkout, &held)?;
            }
        }
        Ok(())
    }

    /// Renames `held` to the place of `checkout`, and repairs a worktree's
    /// entry in its repository, which git may have pointed at `held` when
    /// it read the checkout there.
    fn move_back(
        &self,
        workspace: &Workspace,
        checkout: &Checkout,
        held: &Path,
    ) -> Result<(), Error> {
        store::move_aside(held, &checkout.path)?;
        if workspace.kind == WorkspaceKind::Worktree && checkout.source.is_dir() {
            self.backend
                .repair_checkout(&checkout.source, &checkout.path)?;
        }
        Ok(())
    }

    /// Returns the failure when the commit that `--wip` makes in `plan`
    /// could not run. The caller calls this before it stops any process.
    ///
    /// # Errors
    ///
    /// Returns the reason that a commit could not run.
    pub fn check_wip(&self, plan: &ArchivePlan) -> Result<(), Error> {
        if !plan.options.wip {
            return Ok(());
        }
        for planned in &plan.removal.checkouts {
            let path = &planned.checkout.path;
            if planned.directory == Fate::Removed && !self.backend.changes(path)?.changed.is_empty()
            {
                self.backend.check_commit_all(path)?;
            }
        }
        Ok(())
    }

    /// Commits the changes of each checkout that `confirmed` retires.
    fn commit_wip(&self, confirmed: &ArchivePlan) -> Result<(), Error> {
        for planned in &confirmed.removal.checkouts {
            let path = &planned.checkout.path;
            if planned.directory == Fate::Removed && !self.backend.changes(path)?.changed.is_empty()
            {
                self.backend.commit_all(path, WIP_MESSAGE)?;
            }
        }
        (self.checkpoint)("wip")
    }

    /// Returns what each checkout that `confirmed` retires holds now, by
    /// folder, which later states of the checkout are compared with.
    fn baselines(&self, confirmed: &ArchivePlan) -> Result<BTreeMap<String, Snapshot>, Error> {
        let mut found = BTreeMap::new();
        for planned in &confirmed.removal.checkouts {
            if planned.directory == Fate::Removed {
                found.insert(
                    planned.checkout.name.clone(),
                    self.snapshot(
                        confirmed.removal.workspace.kind,
                        &planned.checkout.path,
                        planned,
                    )?,
                );
            }
        }
        Ok(found)
    }

    /// Carries out `confirmed`, which [`Workspaces::plan_archive`] returned
    /// and the caller confirmed. The caller holds the workspace's lock and
    /// has stopped its processes.
    ///
    /// # Errors
    ///
    /// Returns the failures of the backend's commands and of the writes.
    /// The record stays, so the same command finishes the archive.
    pub fn archive(
        &self,
        archives: &Archives,
        confirmed: ArchivePlan,
    ) -> Result<ArchiveOutcome, Error> {
        let name = confirmed.removal.workspace.name.clone();
        let mut options = confirmed.options;
        if options.wip {
            self.commit_wip(&confirmed)?;
            options.wip = false;
        }
        // What counts is what the checkouts hold from here on: anything that
        // appears later, even between the pin and the rename, refuses.
        let admitted = self.baselines(&confirmed)?;
        // The workspace may have changed while the caller confirmed, and
        // the processes that the caller stopped may have written files, so
        // what counts is what is there now.
        let plan = match self.admit(
            archives,
            &name,
            options,
            Mode::Change,
            Some(confirmed.id.clone()),
            false,
        )? {
            ArchivePlanned::Ready(plan) => *plan,
            ArchivePlanned::Refused(refusal) => return Ok(ArchiveOutcome::Refused(refusal)),
            ArchivePlanned::Finish(_) => {
                return Ok(ArchiveOutcome::Refused(ArchiveRefusal::NotFound));
            }
        };
        if let Some(new) = plan.lost.iter().find(|item| !confirmed.lost.contains(item)) {
            return Ok(ArchiveOutcome::Refused(ArchiveRefusal::SourceChanged {
                folder: name.to_string(),
                differences: vec![format!("--force would lose more than mxv showed: {new}")],
            }));
        }
        let retiring = || {
            plan.removal
                .checkouts
                .iter()
                .zip(&plan.checkouts)
                .filter(|(planned, _)| planned.directory == Fate::Removed)
        };
        for (_, archived) in retiring() {
            let baseline = admitted
                .get(&archived.folder)
                .and_then(|snapshot| snapshot.head.commit.as_deref());
            if baseline != Some(archived.commit.as_str()) {
                return Ok(ArchiveOutcome::Refused(ArchiveRefusal::SourceChanged {
                    folder: archived.folder.clone(),
                    differences: vec![format!(
                        "HEAD moved from {baseline:?} to {} while mxv admitted it",
                        archived.commit
                    )],
                }));
            }
        }
        // The record comes before the pins, so a run that stops between
        // them leaves the archive's id, and the next run writes the same
        // pins again instead of leaving these behind under an id nothing
        // names.
        let mut archive = plan.record(&name);
        archives.write(&archive)?;
        (self.checkpoint)("record")?;
        for (planned, archived) in retiring() {
            self.pin(planned, archived)?;
        }
        (self.checkpoint)("pins")?;
        let generated = self.generated_agents(&plan.removal.workspace)?;
        root::copy_in(archives, &mut archive, &plan.removal.workspace, &generated)?;
        archives.write(&archive)?;
        (self.checkpoint)("root")?;
        for planned in &plan.removal.checkouts {
            if let Some(admitted) = admitted.get(&planned.checkout.name)
                && planned.directory == Fate::Removed
                && let Some(refusal) = self.retire(&plan.removal, planned, admitted)?
            {
                return Ok(ArchiveOutcome::Refused(refusal));
            }
        }
        let differences = root::differences(&archive, &plan.removal.workspace, &generated)?;
        if !differences.is_empty() {
            return Ok(ArchiveOutcome::Refused(ArchiveRefusal::SourceChanged {
                folder: "root".to_owned(),
                differences,
            }));
        }
        root::delete_originals(&archive, &plan.removal.workspace, &generated)?;
        (self.checkpoint)("delete")?;
        self.finish_removal(&name, archive, plan.removal)
    }

    /// Removes the workspace whose checkouts are retired and whose extra
    /// files are deleted.
    fn finish_removal(
        &self,
        name: &WorkspaceName,
        archive: Archive,
        mut removal: Removal,
    ) -> Result<ArchiveOutcome, Error> {
        for planned in &mut removal.checkouts {
            // A worktree's branch stays in its repository, and the pin
            // keeps a commit that a later change of the branch abandons.
            planned.branch = BranchFate::KeptUnmerged { commits: None };
            planned.tip = None;
        }
        match self.remove(removal)? {
            RemovalOutcome::Done(removal) => {
                (self.checkpoint)("removed")?;
                Ok(ArchiveOutcome::Retired {
                    archive: Box::new(archive),
                    removal: Box::new(removal),
                })
            }
            RemovalOutcome::Changed(fresh) => {
                Ok(ArchiveOutcome::Refused(ArchiveRefusal::SourceChanged {
                    folder: name.to_string(),
                    differences: vec![format!(
                        "the workspace changed while mxv removed it: {fresh:?}"
                    )],
                }))
            }
        }
    }

    /// Marks `archive`, whose workspace is gone, as finished.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unwritable`] when the record cannot be written.
    pub fn finish_archive(&self, archives: &Archives, archive: &mut Archive) -> Result<(), Error> {
        archive.state = ArchiveState::Archived;
        archives.write(archive)
    }

    /// Writes the pin of `archived`, the checkout that `planned` retires,
    /// and checks that it names the checkout's commit. The commit of a
    /// clone may exist only in the clone, so it is fetched into the mirror
    /// first, while the mirror's lock is held.
    fn pin(&self, planned: &CheckoutRemoval, archived: &ArchivedCheckout) -> Result<(), Error> {
        let Some(mirror) = &archived.mirror else {
            return self
                .backend
                .write_ref(&archived.source, &archived.pin, &archived.commit);
        };
        let _lock = self.lock_mirror(mirror)?;
        let pinned =
            self.backend
                .fetch_head_into_mirror(mirror, &planned.checkout.path, &archived.pin)?;
        if pinned == archived.commit {
            return Ok(());
        }
        Err(Error::CommandFailed {
            operation: "fetch into mirror",
            dir: mirror.clone(),
            code: None,
            message: format!(
                "the pin {} names {pinned}, and the checkout's HEAD was {}",
                archived.pin, archived.commit
            ),
        })
    }

    /// Reads the git state and the path list of a checkout at `dir`, which
    /// `planned` describes.
    fn snapshot(
        &self,
        kind: WorkspaceKind,
        dir: &Path,
        planned: &CheckoutRemoval,
    ) -> Result<Snapshot, Error> {
        let mut head = self.backend.head(dir)?;
        // The top level is the directory itself, which moves.
        head.toplevel = None;
        let changes = self.backend.changes(dir)?;
        let concealed = self.backend.concealed(dir)?;
        let mut hidden = concealed.nested.clone();
        hidden.extend(concealed.hidden.iter().cloned());
        hidden.extend(submodule_items(dir, &concealed));
        hidden.extend(concealed.unlisted.iter().cloned());
        hidden.extend(concealed.paused.iter().cloned());
        hidden.extend(concealed.referenced.iter().cloned());
        let (stashes, unpushed) = if kind == WorkspaceKind::Clone {
            (
                self.backend.stashes(dir)?,
                self.backend.unpushed_commits(dir)?,
            )
        } else {
            (Vec::new(), Vec::new())
        };
        let trimmed: Vec<&str> = planned
            .disposable
            .iter()
            .map(|path| path.trim_end_matches('/'))
            .collect();
        let skip: BTreeSet<&str> = trimmed.into_iter().collect();
        let mut paths = BTreeSet::new();
        collect_paths(dir, dir, &skip, &mut paths)?;
        Ok(Snapshot {
            head,
            changed: changes.changed,
            ignored: changes.ignored,
            stashes,
            unpushed,
            concealed: hidden,
            paths,
        })
    }

    /// Moves the checkout that `planned` describes aside, compares it with
    /// `admitted`, and deletes it. It puts a checkout that differs back in
    /// its place and returns the refusal.
    fn retire(
        &self,
        removal: &Removal,
        planned: &CheckoutRemoval,
        admitted: &Snapshot,
    ) -> Result<Option<ArchiveRefusal>, Error> {
        let checkout = &planned.checkout;
        let invalid = || Error::Unwritable {
            path: checkout.path.clone(),
            source: io::ErrorKind::InvalidInput.into(),
        };
        let held = quarantine(&checkout.path).ok_or_else(invalid)?;
        let doomed = aside(&checkout.path).ok_or_else(invalid)?;
        delete_directory(&doomed)?;
        if planned.unlock {
            self.backend
                .unlock_checkout(&checkout.source, &checkout.path)?;
        }
        (self.checkpoint)(&format!("before-move:{}", checkout.name))?;
        store::move_aside(&checkout.path, &held)?;
        (self.checkpoint)(&format!("moved:{}", checkout.name))?;
        let moved = match self.snapshot(removal.workspace.kind, &held, planned) {
            Ok(moved) => moved,
            Err(error) => {
                self.move_back(&removal.workspace, checkout, &held)?;
                return Err(error);
            }
        };
        let differences = admitted.differences(&moved);
        if !differences.is_empty() {
            self.move_back(&removal.workspace, checkout, &held)?;
            return Ok(Some(ArchiveRefusal::SourceChanged {
                folder: checkout.name.clone(),
                differences,
            }));
        }
        store::move_aside(&held, &doomed)?;
        (self.checkpoint)(&format!("cleared:{}", checkout.name))?;
        delete_directory(&doomed)?;
        if removal.workspace.kind == WorkspaceKind::Worktree && checkout.source.is_dir() {
            self.backend.prune_checkouts(&checkout.source)?;
        }
        (self.checkpoint)(&format!("retired:{}", checkout.name))?;
        Ok(None)
    }
}
