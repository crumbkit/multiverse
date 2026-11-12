//! Carrying out an archive: writing the record, committing the uncommitted
//! work, pinning what the archive keeps, keeping the ignored files, copying
//! the root files, and retiring each checkout.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use crate::archive::admit::{ArchivePlan, ArchivePlanned, ArchiveRefusal, submodule_items};
use crate::archive::{
    Archive, ArchiveState, ArchivedCheckout, Archives, KeptWip, WIP_MESSAGE, quarantine,
};
use crate::archive::{IgnoredEntry, IgnoredSource, ignored, root};
use crate::backend::{Backend, Head, RefHolder, WipTrees};
use crate::disk::DisposablePath;
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
        /// Each disposable directory that the archive deleted, with its
        /// size measured before the checkouts moved.
        disposable: Vec<DisposablePath>,
        /// Each entry that appeared at the top level of a disposable
        /// directory after the measurement and that the archive deleted
        /// with the directory, with the checkout's folder in front, such as
        /// `app: target/late.o`.
        late: Vec<String>,
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
    /// Each reference that the archive reads, with the object it names: a
    /// clone's every reference, and a worktree's per-worktree references.
    refs: Vec<String>,
    /// The trees of the index and of the working files, which the commits
    /// of the uncommitted work hold.
    wip: WipTrees,
    /// Each path below the checkout, outside `.git` and the disposable
    /// directories.
    paths: BTreeSet<String>,
    /// Each entry at the top level of a disposable directory, such as
    /// `target/debug`. The compare leaves these out, since a build may
    /// change them; [`Snapshot::late_disposable`] names the new ones.
    disposable: BTreeSet<String>,
}

impl Snapshot {
    /// Returns each entry at the top level of a disposable directory that
    /// `later` holds and this snapshot does not, which deleting the
    /// directory deletes with it.
    fn late_disposable(&self, later: &Self) -> Vec<String> {
        later
            .disposable
            .difference(&self.disposable)
            .cloned()
            .collect()
    }

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
        if self.wip != later.wip {
            found.push(format!(
                "the uncommitted work changed from the trees {} and {} to {} and {}",
                self.wip.index, self.wip.worktree, later.wip.index, later.wip.worktree
            ));
        }
        let lists = [
            ("change", &self.changed, &later.changed),
            ("ignored path", &self.ignored, &later.ignored),
            ("stash", &self.stashes, &later.stashes),
            ("commit", &self.unpushed, &later.unpushed),
            ("hidden work", &self.concealed, &later.concealed),
            ("reference", &self.refs, &later.refs),
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

/// What the plan and the record say about one checkout that an archive
/// retires, which the compare after the move reads.
struct Retiring<'a> {
    /// The files whose uncommitted content the archive leaves out.
    left_out: &'a [String],
    /// The ignored entries that the plan keeps of the checkout.
    sources: &'a [IgnoredSource],
    /// The ignored entries that the record and the tar keep of the checkout.
    kept: &'a [IgnoredEntry],
}

impl Retiring<'_> {
    /// Returns one sentence for each ignored entry of the checkout moved to
    /// `held` that differs from what the tar keeps, or is gone.
    fn ignored_differences(&self, held: &Path) -> Result<Vec<String>, Error> {
        let moved = ignored::on_disk(self.sources, held);
        Ok(ignored::changed(self.kept, &moved)?
            .into_iter()
            .map(|path| {
                let relative = path.split_once('/').map_or(path.as_str(), |(_, rest)| rest);
                format!("the ignored file {relative} changed after the archive kept it")
            })
            .collect())
    }
}

/// Returns each checkout that `plan` retires, with its entry in `archive`,
/// whose checkouts are in the plan's order.
fn retiring_in<'a>(
    plan: &'a ArchivePlan,
    archive: &'a Archive,
) -> impl Iterator<Item = (&'a CheckoutRemoval, &'a ArchivedCheckout)> {
    plan.removal
        .checkouts
        .iter()
        .zip(&archive.checkouts)
        .filter(|(planned, _)| planned.directory == Fate::Removed)
}

/// Adds to `paths` each path below `dir`, relative to `root`, other than
/// `.git` at the top, without following a symbolic link. A directory in
/// `skip`, which holds the disposable directories, is added itself and not
/// walked, so the compare notices a disposable directory that goes and
/// leaves what a build writes into one alone. The entries at the top level
/// of each such directory go in `inside`, so the archive can name an entry
/// that a process writes there after the measurement.
fn collect_paths(
    root: &Path,
    dir: &Path,
    skip: &BTreeSet<&str>,
    paths: &mut BTreeSet<String>,
    inside: &mut BTreeSet<String>,
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
        if relative == ".git" {
            continue;
        }
        let is_dir = entry.file_type().map_err(unreadable)?.is_dir();
        let disposable = skip.contains(relative.as_str());
        if is_dir && disposable {
            collect_top_level(root, &path, inside)?;
        } else if is_dir {
            collect_paths(root, &path, skip, paths, inside)?;
        }
        paths.insert(relative);
    }
    Ok(())
}

/// Adds to `inside` each entry at the top level of the disposable
/// directory `dir`, relative to `root`.
fn collect_top_level(root: &Path, dir: &Path, inside: &mut BTreeSet<String>) -> Result<(), Error> {
    let unreadable = |source| Error::StateUnreadable {
        path: dir.to_path_buf(),
        source,
    };
    for entry in fs::read_dir(dir).map_err(unreadable)? {
        let path = entry.map_err(unreadable)?.path();
        let relative = path.strip_prefix(root).unwrap_or(&path);
        inside.insert(relative.to_string_lossy().into_owned());
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

    /// Returns what each checkout that `confirmed` retires holds now, by
    /// folder, which later states of the checkout are compared with. The
    /// uncommitted work is read through `index`.
    fn baselines(
        &self,
        confirmed: &ArchivePlan,
        index: &Path,
    ) -> Result<BTreeMap<String, Snapshot>, Error> {
        let mut found = BTreeMap::new();
        for planned in &confirmed.removal.checkouts {
            if planned.directory == Fate::Removed {
                found.insert(
                    planned.checkout.name.clone(),
                    self.snapshot(
                        confirmed.removal.workspace.kind,
                        &planned.checkout.path,
                        planned,
                        confirmed.left_out(&planned.checkout.name),
                        index,
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
        let index = archives.wip_index(&name)?;
        // What counts is what the checkouts hold from here on: anything that
        // appears later, even between the pin and the rename, refuses.
        let admitted = self.baselines(&confirmed, &index)?;
        // The workspace may have changed while the caller confirmed, and
        // the processes that the caller stopped may have written files, so
        // what counts is what is there now.
        let plan = match self.admit(
            archives,
            &name,
            confirmed.options,
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
        self.commit_wip(&plan, &mut archive, &index)?;
        archives.write(&archive)?;
        (self.checkpoint)("wip")?;
        for (planned, archived) in retiring_in(&plan, &archive) {
            self.pin(planned, archived)?;
        }
        (self.checkpoint)("pins")?;
        // The record already carries the entries of the checkouts that an
        // earlier run retired, which only the tar holds now; the standing
        // checkouts' files are written again and the carried ones copied.
        let sources = plan.ignored_sources();
        if !sources.is_empty() {
            let carried = archive.ignored.clone();
            archive.ignored =
                ignored::keep_carrying(&archives.data_of(&archive.id), &sources, &carried)?;
            archives.write(&archive)?;
        }
        (self.checkpoint)("ignored")?;
        let generated = self.generated_agents(&plan.removal.workspace)?;
        root::copy_in(archives, &mut archive, &plan.removal.workspace, &generated)?;
        archives.write(&archive)?;
        (self.checkpoint)("root")?;
        let mut late = Vec::new();
        for (planned, archived) in retiring_in(&plan, &archive) {
            let folder = planned.checkout.name.as_str();
            let kept: Vec<IgnoredEntry> = archive
                .ignored
                .iter()
                .filter(|entry| entry.folder() == folder)
                .cloned()
                .collect();
            let Some(admitted) = admitted.get(folder) else {
                continue;
            };
            let retired = self.retire(
                &plan.removal,
                planned,
                archived,
                admitted,
                Retiring {
                    left_out: plan.left_out(folder),
                    sources: &plan.ignored_in(folder),
                    kept: &kept,
                },
                &index,
            )?;
            match retired {
                Ok(found) => late.extend(found),
                Err(refusal) => return Ok(ArchiveOutcome::Refused(refusal)),
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
        self.finish_removal(&name, archive, plan.removal, plan.disposable, late)
    }

    /// Writes the commits of the uncommitted work of each checkout that
    /// `plan` retires and that has any, and names them in `archive`. The
    /// commits take the archive's time, so a later run writes the same
    /// commits for the same work.
    fn commit_wip(
        &self,
        plan: &ArchivePlan,
        archive: &mut Archive,
        index: &Path,
    ) -> Result<(), Error> {
        let time = archive.time;
        for (planned, archived) in plan.removal.checkouts.iter().zip(&mut archive.checkouts) {
            if planned.directory != Fate::Removed {
                continue;
            }
            let has_work = plan
                .uncommitted
                .iter()
                .any(|work| work.folder == planned.checkout.name && !work.changes.is_empty());
            archived.kept.wip = if has_work {
                let written = self.backend.write_wip(
                    &planned.checkout.path,
                    index,
                    plan.left_out(&planned.checkout.name),
                    time,
                    WIP_MESSAGE,
                )?;
                Some(KeptWip {
                    index: written.index,
                    worktree: written.worktree,
                })
            } else {
                None
            };
        }
        Ok(())
    }

    /// Removes the workspace whose checkouts are retired and whose extra
    /// files are deleted, and names in the outcome the disposable
    /// directories `disposable` that went with the checkouts and the
    /// entries `late` that appeared in them after they were measured.
    fn finish_removal(
        &self,
        name: &WorkspaceName,
        archive: Archive,
        mut removal: Removal,
        disposable: Vec<DisposablePath>,
        late: Vec<String>,
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
                    disposable,
                    late,
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

    /// Writes every pin of `archived`, the checkout that `planned` retires,
    /// and deletes a pin below its pin directory that an earlier run wrote
    /// and the record no longer names. The objects of a clone may exist
    /// only in the clone, so they are fetched into the mirror, while the
    /// mirror's lock is held, and the fetch checks what each pin names.
    fn pin(&self, planned: &CheckoutRemoval, archived: &ArchivedCheckout) -> Result<(), Error> {
        let pins = archived.pins();
        let holder = archived.pin_holder();
        let _lock = match &archived.mirror {
            Some(mirror) => Some(self.lock_mirror(mirror)?),
            None => None,
        };
        // git matches the directory as a whole component, so this lists the
        // pins below it and a pin of version 1 at the directory itself,
        // which an earlier run of an older multiverse wrote and which would
        // stop git from writing any pin below it.
        let dir = archived.pin_dir();
        for (rest, _) in self.backend.refs_under(holder, dir)? {
            let reference = format!("{dir}{rest}");
            if !pins.iter().any(|(pin, _)| *pin == reference) {
                self.backend.delete_ref(holder, &reference)?;
            }
        }
        match holder {
            RefHolder::Mirror(mirror) => {
                self.backend
                    .fetch_into_mirror(mirror, &planned.checkout.path, &pins)
            }
            RefHolder::Repository(source) => {
                for (reference, object) in &pins {
                    self.backend.write_ref(source, reference, object)?;
                }
                Ok(())
            }
        }
    }

    /// Reads the git state and the path list of a checkout at `dir`, which
    /// `planned` describes, reading the uncommitted work without
    /// `left_out` through `index`.
    fn snapshot(
        &self,
        kind: WorkspaceKind,
        dir: &Path,
        planned: &CheckoutRemoval,
        left_out: &[String],
        index: &Path,
    ) -> Result<Snapshot, Error> {
        let mut head = self.backend.head(dir)?;
        // The top level is the directory itself, which moves.
        head.toplevel = None;
        let changes = self.backend.changes(dir)?;
        let concealed = self.backend.concealed(dir)?;
        let mut hidden = concealed.nested.clone();
        hidden.extend(concealed.hidden.iter().map(|item| item.path.clone()));
        hidden.extend(
            submodule_items(dir, &concealed)
                .into_iter()
                .map(|(path, work)| format!("{path}: {work}")),
        );
        hidden.extend(concealed.unlisted.iter().cloned());
        hidden.extend(
            concealed
                .paused
                .iter()
                .map(|paused| format!("{}{:?}", paused.at, paused.operation)),
        );
        let own = RefHolder::Repository(dir);
        let (stashes, unpushed, refs) = if kind == WorkspaceKind::Clone {
            (
                self.backend.stashes(dir)?,
                self.backend.unpushed_commits(dir)?,
                self.backend.refs_under(own, "refs/")?,
            )
        } else {
            (
                Vec::new(),
                Vec::new(),
                self.backend.refs_under(own, "refs/worktree/")?,
            )
        };
        let wip = self.backend.wip_trees(dir, index, left_out)?;
        let trimmed: Vec<&str> = planned
            .disposable
            .iter()
            .map(|path| path.trim_end_matches('/'))
            .collect();
        let skip: BTreeSet<&str> = trimmed.into_iter().collect();
        let mut paths = BTreeSet::new();
        let mut disposable = BTreeSet::new();
        collect_paths(dir, dir, &skip, &mut paths, &mut disposable)?;
        Ok(Snapshot {
            head,
            changed: changes.changed,
            ignored: changes.ignored,
            stashes,
            unpushed,
            concealed: hidden,
            refs: refs
                .into_iter()
                .map(|(name, object)| format!("{name} {object}"))
                .collect(),
            wip,
            paths,
            disposable,
        })
    }

    /// Returns what differs between the uncommitted work of the checkout
    /// at `dir`, whose snapshot is `moved`, and the commits that `archived`
    /// keeps of it, one sentence for each difference. A checkout without
    /// kept work must have none: its index and working files hold the tree
    /// of `HEAD`.
    fn kept_work_differences(
        &self,
        dir: &Path,
        archived: &ArchivedCheckout,
        moved: &Snapshot,
    ) -> Result<Vec<String>, Error> {
        let (index, worktree) = match &archived.kept.wip {
            Some(wip) => (
                self.backend.tree_of(dir, &wip.index)?,
                self.backend.tree_of(dir, &wip.worktree)?,
            ),
            None => {
                let head = self.backend.tree_of(dir, &archived.commit)?;
                (head.clone(), head)
            }
        };
        let mut found = Vec::new();
        if moved.wip.index != index {
            found.push(format!(
                "the index holds the tree {}, and the archive kept {index}",
                moved.wip.index
            ));
        }
        if moved.wip.worktree != worktree {
            found.push(format!(
                "the working files hold the tree {}, and the archive kept {worktree}",
                moved.wip.worktree
            ));
        }
        Ok(found)
    }

    /// Moves the checkout that `planned` describes aside, compares it with
    /// `admitted`, with what `archived` keeps of it and with the ignored
    /// entries that the tar keeps of it, and deletes it. It puts a checkout
    /// that differs back in its place and returns the refusal. Otherwise it
    /// returns each entry that appeared at the top level of a disposable
    /// directory after `admitted` was taken, with the folder in front, such
    /// as `app: target/late.o`, which the deletion deleted. The
    /// uncommitted work is read without `retiring.left_out` through
    /// `index`.
    fn retire(
        &self,
        removal: &Removal,
        planned: &CheckoutRemoval,
        archived: &ArchivedCheckout,
        admitted: &Snapshot,
        retiring: Retiring<'_>,
        index: &Path,
    ) -> Result<Result<Vec<String>, ArchiveRefusal>, Error> {
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
        let compared = self
            .snapshot(
                removal.workspace.kind,
                &held,
                planned,
                retiring.left_out,
                index,
            )
            .and_then(|moved| {
                let mut differences = admitted.differences(&moved);
                differences.extend(self.kept_work_differences(&held, archived, &moved)?);
                differences.extend(retiring.ignored_differences(&held)?);
                Ok((differences, admitted.late_disposable(&moved)))
            });
        let (differences, late) = match compared {
            Ok(found) => found,
            Err(error) => {
                self.move_back(&removal.workspace, checkout, &held)?;
                return Err(error);
            }
        };
        if !differences.is_empty() {
            self.move_back(&removal.workspace, checkout, &held)?;
            return Ok(Err(ArchiveRefusal::SourceChanged {
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
        Ok(Ok(late
            .into_iter()
            .map(|path| format!("{}: {path}", checkout.name))
            .collect()))
    }
}
