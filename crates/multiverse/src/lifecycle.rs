//! The order of a destructive command: lock the workspace's name, read its
//! records again, compare them with what the person confirmed, and only then
//! stop, delete or write.

use crate::archive::{
    Archive, ArchiveOptions, ArchiveOutcome, ArchivePlan, ArchivePlanned, ArchiveRefusal,
    ArchiveState, ArchivedRemoval, Archives, RestoreOptions, RestoreOutcome, RestorePlan,
    RestorePlanned, RestoreRefusal, Restored,
};
use crate::disk::DisposablePath;
use crate::error::Error;
use crate::health::{Rebuilt, Repaired, rebuilt};
use crate::name::WorkspaceName;
use crate::record::Workspace;
use crate::runs::{DownOutcome, DownResult, Needed, Runs, Unstoppable, Volumes, WorkspaceLock};
use crate::workspaces::{Mode, Removal, RemovalOutcome, RemovalPlan, Workspaces};

#[cfg(test)]
mod tests;

/// The destructive commands of one workspace store, each of which holds the
/// workspace's lock from its last check to its last write.
#[derive(Debug)]
pub struct Lifecycle<'a, B> {
    /// The workspaces and their records.
    workspaces: &'a Workspaces<B>,
    /// The archives and their records.
    archives: &'a Archives,
    /// The run files, the docker volumes and the lock of each workspace.
    runs: &'a Runs,
    /// The check for a process outside phase that works in a workspace.
    in_use: InUse,
}

/// Returns why a process outside phase keeps `workspace` from being
/// archived, or `None` when no such process works in it.
pub type InUse = fn(&Workspace) -> Result<Option<String>, Error>;

/// The in-use check of a [`Lifecycle`] that its caller gave none: no process
/// outside phase keeps the workspace.
const fn nobody(_workspace: &Workspace) -> Result<Option<String>, Error> {
    Ok(None)
}

/// What the person's answer to the question of a removal allowed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoveAsked {
    /// Whether `--force` allowed the removal to stop the running units.
    force: bool,
    /// The docker volumes that the question named, which the removal may
    /// delete.
    volumes: Vec<String>,
}

impl RemoveAsked {
    /// Returns what a question allowed: stopping the running units when
    /// `force` is set, and deleting the docker volumes `volumes`.
    #[must_use]
    pub const fn new(force: bool, volumes: Vec<String>) -> Self {
        Self { force, volumes }
    }
}

/// A removal that ran.
#[derive(Debug)]
#[non_exhaustive]
pub struct RemovalDone {
    /// What the removal did to the checkouts and the record.
    pub removal: Removal,
    /// The sentences that say why phase kept the workspace's run record,
    /// which is empty when phase deleted it.
    pub held: Vec<String>,
}

/// How [`Lifecycle::remove`] ended without an error.
#[derive(Debug)]
pub enum Removed {
    /// The workspace, its record, its run files and its volumes are gone.
    Done(RemovalDone),
    /// No record has the name, so nothing was stopped or deleted.
    Unchanged,
    /// A new plan refuses the removal, or would lose more than the
    /// confirmed plan showed, and nothing was stopped or deleted before it.
    Refused(RemovalPlan),
    /// The record differs from the confirmed one, or docker holds a volume
    /// that the question did not name, and nothing was stopped or deleted.
    Changed,
    /// Units run that the question did not allow the removal to stop, and
    /// nothing was stopped or deleted.
    Running(Vec<String>),
    /// A process outlived the stop, so the workspace and its record stay.
    Unstoppable(Unstoppable),
}

/// The docker volumes, and for an archive the running units, that the
/// question of an archive or of its deletion named, which the command may
/// delete and stop.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VolumesAsked {
    /// The volumes' docker names, or none when the question named none.
    volumes: Vec<String>,
    /// The units that the question of an archive said it stops.
    units: Vec<String>,
}

impl VolumesAsked {
    /// Returns what a question that named the docker volumes `volumes`
    /// allowed. With none, the command reads and deletes no volume.
    #[must_use]
    pub const fn new(volumes: Vec<String>) -> Self {
        Self {
            volumes,
            units: Vec::new(),
        }
    }

    /// Returns what the question allowed, and also that the archive stops
    /// the running units `units`. A unit that runs later and that the
    /// question did not name stops nothing and archives nothing.
    #[must_use]
    pub fn with_units(mut self, units: Vec<String>) -> Self {
        self.units = units;
        self
    }
}

/// How [`Lifecycle::delete_archive`] ended without an error.
#[derive(Debug)]
pub enum ArchiveDeleted {
    /// The archive, its pins and the volumes that the question named are
    /// gone.
    Done(Box<ArchivedRemoval>),
    /// No archive has the name, so nothing was deleted.
    Unchanged,
    /// The archive of the name is another one than the question was about,
    /// is no longer finished, or docker holds a volume that the question did
    /// not name, and nothing was deleted.
    Changed,
    /// The live workspace of this name has the stable id of the archive, so
    /// the docker volumes are its own, and nothing was deleted.
    VolumesInUse(WorkspaceName),
}

/// What the finish of an archive did.
#[derive(Debug)]
pub struct Finish {
    /// The names of the docker volumes that the finish deleted.
    pub volumes: Vec<String>,
    /// The sentences that say why phase kept the workspace's run record,
    /// which is empty when phase deleted it.
    pub held: Vec<String>,
}

/// An archive that [`Lifecycle::finish`] marked finished.
#[derive(Debug)]
pub struct FinishDone {
    /// The archive record as the finish wrote it.
    pub archive: Archive,
    /// What the finish deleted.
    pub finish: Finish,
}

/// How [`Lifecycle::finish`] ended without an error.
#[derive(Debug)]
pub enum Finished {
    /// The archive's run files and the volumes that the question named are
    /// gone, and the archive is marked finished.
    Done(Box<FinishDone>),
    /// The archive of the name has finished already, so there is nothing to
    /// do.
    Unchanged(Box<Archive>),
    /// The archive of the name is another one than the question was about,
    /// the name has a workspace again, or docker holds a volume that the
    /// question did not name, and nothing was deleted.
    Changed,
    /// The archive cannot be finished, and nothing was deleted.
    Refused(ArchiveRefusal),
}

/// An archive that [`Lifecycle::archive`] retired and finished.
#[derive(Debug)]
pub struct RetireDone {
    /// What the removal of the workspace did.
    pub removal: Removal,
    /// Each disposable directory that the archive deleted.
    pub disposable: Vec<DisposablePath>,
    /// Each entry that appeared in a disposable directory after the
    /// measurement and that the archive deleted with it.
    pub late: Vec<String>,
    /// The finished archive and what its finish deleted.
    pub finished: FinishDone,
}

/// How [`Lifecycle::archive`] ended without an error.
#[derive(Debug)]
pub enum Archived {
    /// The workspace is retired, its run files and the volumes that the
    /// question named are gone, and the archive is finished.
    Done(Box<RetireDone>),
    /// An earlier run retired the workspace's checkouts and stopped, and
    /// this run finished that archive.
    Finished(Box<FinishDone>),
    /// The archive of the name has finished already, so there is nothing to
    /// do.
    Unchanged(Box<Archive>),
    /// The new plan refuses the archive, and nothing was stopped or
    /// deleted.
    Refused(ArchiveRefusal),
    /// A unit runs that the question did not name, docker holds a volume
    /// that it did not name, or the archive of the name belongs to another
    /// workspace, and nothing was stopped or deleted.
    Changed,
    /// A process outside phase works in the workspace for the reason given,
    /// so the archive stopped after it stopped the units.
    InUse(String),
    /// A process outlived the stop, so the workspace stays.
    Unstoppable(Unstoppable),
}

/// How [`Lifecycle::restore`] ended without an error.
#[derive(Debug)]
pub enum Restoration {
    /// The workspace is back, and the archive is deleted.
    Done(Box<Restored>),
    /// A workspace of the archive's stable id holds the name already, or no
    /// archive of the name needs restoring, so there is nothing to do. An
    /// archive that an earlier restore left stays until `mxv restore` or
    /// `mxv repair` deletes it.
    Unchanged,
    /// The archive of the name is another one than the confirmed plan was
    /// about, and nothing was built.
    Changed,
    /// The restore cannot go ahead, and nothing was built.
    Refused(RestoreRefusal),
}

/// What [`Lifecycle::repair`] did.
#[derive(Debug)]
#[non_exhaustive]
pub struct RepairDone {
    /// What the repair did.
    pub repaired: Repaired,
    /// What the finish of a renamed archive deleted, or `None` when the
    /// repair renamed no archive.
    pub finish: Option<Finish>,
}

/// How [`Lifecycle::repair`] ended without an error.
#[derive(Debug)]
pub enum Repairing {
    /// The repair ran.
    Done(Box<RepairDone>),
    /// The records plan another repair than the confirmed one, and nothing
    /// was changed. It also ends a rename that chose another dated name
    /// than the second plan, after the rename and before the finish.
    Changed,
}

impl<'a, B> Lifecycle<'a, B> {
    /// Returns the destructive commands of `workspaces` and `archives`,
    /// whose run files, volumes and locks `runs` holds. An archive checks
    /// no process outside phase until [`Lifecycle::with_in_use`] gives it
    /// the check.
    #[must_use]
    pub const fn new(
        workspaces: &'a Workspaces<B>,
        archives: &'a Archives,
        runs: &'a Runs,
    ) -> Self {
        Self {
            workspaces,
            archives,
            runs,
            in_use: nobody,
        }
    }

    /// Returns these commands with `in_use` as the check for a process
    /// outside phase that works in the workspace of an archive.
    #[must_use]
    pub const fn with_in_use(mut self, in_use: InUse) -> Self {
        self.in_use = in_use;
        self
    }
}

impl<B: crate::backend::Backend> Lifecycle<'_, B> {
    /// Removes the workspace that `confirmed` planned. It takes the lock of
    /// the workspace's name, reads the record again, and compares it with
    /// `confirmed` before it stops or deletes anything. Then it stops the
    /// units, deletes the checkouts and the record, deletes the run files,
    /// and deletes the docker volumes, and it holds the lock through the
    /// last deletion.
    ///
    /// The record that differs from the confirmed one, running units that
    /// `asked` did not allow, and a docker volume that `asked` did not name
    /// each end the removal before the first stop.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Busy`] when another command holds the lock for the
    /// whole lock wait, and the errors of [`Workspaces::remove`] and of the
    /// run files and docker. The steps before a failure stay done, and the
    /// lock is released, so the command can run again.
    pub fn remove(&self, confirmed: Removal, asked: &RemoveAsked) -> Result<Removed, Error> {
        let name = &confirmed.workspace.name;
        let lock = self.runs.lock(name)?;
        let Some(record) = self.workspaces.store.read(name)? else {
            return Ok(Removed::Unchanged);
        };
        if record != confirmed.workspace {
            return Ok(Removed::Changed);
        }
        let running = self.runs.running(&record)?;
        if !running.is_empty() && !asked.force {
            return Ok(Removed::Running(running));
        }
        let volumes = self.runs.volumes(&record)?;
        if volumes
            .names()
            .iter()
            .any(|volume| !asked.volumes.contains(volume))
        {
            return Ok(Removed::Changed);
        }
        if !running.is_empty() {
            let outcome = self.runs.down(&lock, &record, None, &[], Needed::Stop)?;
            if let DownOutcome::Down(results) = outcome {
                for result in results {
                    if let DownResult::Unstoppable(unstoppable) = result {
                        return Ok(Removed::Unstoppable(unstoppable));
                    }
                }
            }
        }
        (self.workspaces.checkpoint)("stopped")?;
        let removal = match self.workspaces.remove(confirmed)? {
            RemovalOutcome::Done(removal) => removal,
            RemovalOutcome::Changed(RemovalPlan::NotFound) => return Ok(Removed::Unchanged),
            RemovalOutcome::Changed(plan) => return Ok(Removed::Refused(plan)),
        };
        (self.workspaces.checkpoint)("removed")?;
        let held = self
            .runs
            .forget(&removal.workspace.name, record.id.as_ref())?;
        (self.workspaces.checkpoint)("forgotten")?;
        if !volumes.is_empty() {
            self.runs.delete_volumes(volumes)?;
        }
        (self.workspaces.checkpoint)("volumes")?;
        Ok(Removed::Done(RemovalDone { removal, held }))
    }

    /// Deletes the archive that `confirmed` is. It takes the lock of the
    /// archive's name and calls [`Lifecycle::delete_archive_held`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Busy`] when another command holds the lock for the
    /// whole lock wait, and the errors of [`Lifecycle::delete_archive_held`].
    pub fn delete_archive(
        &self,
        confirmed: &Archive,
        asked: &VolumesAsked,
    ) -> Result<ArchiveDeleted, Error> {
        let lock = self.runs.lock(&confirmed.name)?;
        self.delete_archive_held(&lock, confirmed, asked)
    }

    /// Refuses with [`Error::StoppedRestoreHoldsOtherFiles`] when `archive`
    /// holds the mark of a restore that stopped after it made the
    /// workspace's directory, no workspace holds its name, and the
    /// directory holds an entry that the mark does not account for. A dry
    /// run of `mxv remove --archived` calls it, so it refuses as the
    /// removal would. It changes nothing.
    ///
    /// # Errors
    ///
    /// Returns the refusal, and the errors of reading the workspace record
    /// and the directory.
    pub fn check_stopped_restore(&self, archive: &Archive) -> Result<(), Error> {
        if archive.restoring.is_some() && self.workspaces.store.read(&archive.name)?.is_none() {
            crate::archive::refuse_other_files(archive)?;
        }
        Ok(())
    }

    /// Deletes the archive that `confirmed` is, for a caller that holds the
    /// lock of its name as `lock`, because a second lock of one name in one
    /// process waits and fails with [`Error::Busy`]. It reads the archive of
    /// the name again, and deletes nothing unless that is `confirmed`'s
    /// archive, with the same id, and finished.
    ///
    /// When `asked` names docker volumes, it refuses while a live workspace
    /// has the archive's stable id, and refuses when docker holds a volume of
    /// the archive that `asked` did not name. Then it deletes those volumes,
    /// the pins and the record, in that order, so the record that holds the
    /// volumes' stable id stays until they are gone, and a failure can be
    /// tried again. An archive that holds the mark of a restore that died,
    /// with no workspace of the name, has what that restore made deleted
    /// before its record goes, because the mark goes with the record.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WrongLock`] when `lock` is not the lock of
    /// `confirmed`'s name. Returns
    /// [`Error::StoppedRestoreHoldsOtherFiles`], before it deletes
    /// anything, when the archive holds the mark of a stopped restore whose
    /// directory holds an entry that the mark does not account for, as
    /// [`Lifecycle::check_stopped_restore`] says. Returns the errors of the
    /// archive's record, of docker, and of [`Workspaces::remove_archive`];
    /// when one of those fails, the deletions before it stay done.
    pub fn delete_archive_held(
        &self,
        lock: &WorkspaceLock,
        confirmed: &Archive,
        asked: &VolumesAsked,
    ) -> Result<ArchiveDeleted, Error> {
        lock.check(&confirmed.name)?;
        let Some(archive) = self.archives.read(&confirmed.name)? else {
            return Ok(ArchiveDeleted::Unchanged);
        };
        if archive.id != confirmed.id || archive.state != ArchiveState::Archived {
            return Ok(ArchiveDeleted::Changed);
        }
        // A stopped restore whose directory holds the person's files is
        // refused before the volumes go, so the refusal deletes nothing.
        self.check_stopped_restore(&archive)?;
        let stopped =
            archive.restoring.is_some() && self.workspaces.store.read(&archive.name)?.is_none();
        if !asked.volumes.is_empty() {
            if let Some(workspace) = self.volumes_in_use(&archive)? {
                return Ok(ArchiveDeleted::VolumesInUse(workspace));
            }
            let volumes = match archive
                .workspace_id
                .as_ref()
                .filter(|_| archive.containers_ran)
            {
                Some(id) => Some(self.runs.archived_volumes(id)?),
                None => None,
            };
            if let Some(volumes) = volumes {
                if volumes
                    .names()
                    .iter()
                    .any(|volume| !asked.volumes.contains(volume))
                {
                    return Ok(ArchiveDeleted::Changed);
                }
                if !volumes.is_empty() {
                    self.runs.delete_volumes(volumes)?;
                }
            }
        }
        (self.workspaces.checkpoint)("volumes")?;
        // A restore that died leaves what its mark lists, and the mark goes
        // with the record, so it is undone first. A workspace that holds the
        // name means the restore finished, and the mark is stale.
        let mut archive = archive;
        if stopped {
            self.workspaces
                .undo_stopped_restore(self.archives, &mut archive)?;
        }
        let removed = self
            .workspaces
            .remove_archive(self.archives, archive, Mode::Change)?;
        Ok(ArchiveDeleted::Done(Box::new(removed)))
    }

    /// Plans the archive of the workspace `name`. It takes the lock of the
    /// name, plans in [`Mode::Change`] and drops the lock before it returns,
    /// so no lock is held while the person answers the question.
    ///
    /// The plan needs the lock because it puts a checkout that an earlier
    /// run moved aside back into its place, which would undo the move of
    /// another archive of the name that holds the lock.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Busy`] when another command holds the lock for the
    /// whole lock wait, and the errors of [`Workspaces::plan_archive`].
    pub fn plan_archive(
        &self,
        name: &WorkspaceName,
        options: ArchiveOptions,
    ) -> Result<ArchivePlanned, Error> {
        let _lock = self.runs.lock(name)?;
        self.workspaces
            .plan_archive(self.archives, name, options, Mode::Change)
    }

    /// Archives the workspace that `confirmed` planned. It takes the lock of
    /// the workspace's name and plans again with
    /// [`Workspaces::plan_archive`], which does not lock, before it stops or
    /// deletes anything:
    ///
    /// - A plan that refuses, or that finds the archive finished, ends the
    ///   run with that answer.
    /// - A plan that finds the checkouts retired and the workspace's record
    ///   gone finishes the archive of the confirmed workspace.
    /// - A workspace of the name whose id is not the confirmed one, such as
    ///   one that a removal and a new workspace made while the question was
    ///   open, ends the run with [`Archived::Changed`].
    /// - A unit that runs and that `asked` did not name, and a docker volume
    ///   that `asked` did not name, end the run with [`Archived::Changed`]
    ///   before the first stop.
    ///
    /// Then it stops the units, runs the in-use check, retires the
    /// checkouts and finishes the archive, and it holds the lock through the
    /// last write.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Busy`] when another command holds the lock for the
    /// whole lock wait, and the errors of the plan, the archive, the run
    /// files and docker. The steps before a failure stay done, and the lock
    /// is released, so the command can run again.
    pub fn archive(
        &self,
        mut confirmed: ArchivePlan,
        asked: &VolumesAsked,
    ) -> Result<Archived, Error> {
        let name = confirmed.removal.workspace.name.clone();
        let lock = self.runs.lock(&name)?;
        // Whether containers ran is read again under the lock, because a
        // unit that started after the question may have started some.
        confirmed.options.containers_ran = self.runs.containers_ran(&name);
        let plan = match self.workspaces.plan_archive(
            self.archives,
            &name,
            confirmed.options,
            Mode::Change,
        )? {
            ArchivePlanned::Ready(plan) => *plan,
            ArchivePlanned::Unchanged(archive) => return Ok(Archived::Unchanged(archive)),
            ArchivePlanned::Refused(refusal) => return Ok(Archived::Refused(refusal)),
            ArchivePlanned::Finish(mut archive) => {
                if archive.workspace_id != confirmed.removal.workspace.id {
                    return Ok(Archived::Changed);
                }
                let Some(finish) = self.finish_held(&lock, &mut archive, asked)? else {
                    return Ok(Archived::Changed);
                };
                return Ok(Archived::Finished(Box::new(FinishDone {
                    archive: *archive,
                    finish,
                })));
            }
        };
        let workspace = plan.removal.workspace;
        if workspace.id != confirmed.removal.workspace.id {
            return Ok(Archived::Changed);
        }
        let running = self.runs.running(&workspace)?;
        if running.iter().any(|unit| !asked.units.contains(unit)) {
            return Ok(Archived::Changed);
        }
        let volumes = if asked.volumes.is_empty() {
            None
        } else {
            let volumes = self.runs.volumes(&workspace)?;
            if unnamed(&volumes, &asked.volumes) {
                return Ok(Archived::Changed);
            }
            Some(volumes)
        };
        if !running.is_empty() {
            let outcome = self.runs.down(&lock, &workspace, None, &[], Needed::Stop)?;
            if let DownOutcome::Down(results) = outcome {
                for result in results {
                    if let DownResult::Unstoppable(unstoppable) = result {
                        return Ok(Archived::Unstoppable(unstoppable));
                    }
                }
            }
        }
        if let Some(reason) = (self.in_use)(&workspace)? {
            return Ok(Archived::InUse(reason));
        }
        let (mut archive, removal, disposable, late) =
            match self.workspaces.archive(self.archives, confirmed)? {
                ArchiveOutcome::Retired {
                    archive,
                    removal,
                    disposable,
                    late,
                } => (*archive, *removal, disposable, late),
                ArchiveOutcome::Refused(refusal) => return Ok(Archived::Refused(refusal)),
            };
        let finish = self.finish_archive(&mut archive, volumes)?;
        Ok(Archived::Done(Box::new(RetireDone {
            removal,
            disposable,
            late,
            finished: FinishDone { archive, finish },
        })))
    }

    /// Finishes the archive that `confirmed` is, whose workspace is gone. It
    /// takes the lock of the archive's name, plans the archive again, and
    /// finishes the archive of the name only when that is `confirmed`'s
    /// archive. It deletes the docker volumes that `asked` names, and
    /// refuses with [`Finished::Changed`] when docker holds a volume of the
    /// archive that `asked` did not name.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Busy`] when another command holds the lock for the
    /// whole lock wait, and the errors of the plan, the run files and
    /// docker. The steps before a failure stay done.
    pub fn finish(&self, confirmed: &Archive, asked: &VolumesAsked) -> Result<Finished, Error> {
        let lock = self.runs.lock(&confirmed.name)?;
        // A plan that finds the archive finishable reads nothing but
        // records, so it needs no put back and runs as a dry run.
        let planned = self.workspaces.plan_archive(
            self.archives,
            &confirmed.name,
            ArchiveOptions::default(),
            Mode::DryRun,
        )?;
        match planned {
            ArchivePlanned::Finish(mut archive) if archive.id == confirmed.id => {
                match self.finish_held(&lock, &mut archive, asked)? {
                    Some(finish) => Ok(Finished::Done(Box::new(FinishDone {
                        archive: *archive,
                        finish,
                    }))),
                    None => Ok(Finished::Changed),
                }
            }
            ArchivePlanned::Unchanged(archive) if archive.id == confirmed.id => {
                Ok(Finished::Unchanged(archive))
            }
            ArchivePlanned::Refused(refusal) => Ok(Finished::Refused(refusal)),
            ArchivePlanned::Finish(_) | ArchivePlanned::Unchanged(_) | ArchivePlanned::Ready(_) => {
                Ok(Finished::Changed)
            }
        }
    }

    /// Finishes `archive`, whose workspace is gone, for a caller that holds
    /// the lock of its name as `lock`, because a second lock of one name in
    /// one process waits and fails with [`Error::Busy`]. It deletes the run
    /// files and, when `asked` names docker volumes, the volumes of the
    /// archive, and then marks `archive` finished. It returns `None` and
    /// deletes nothing when docker holds a volume of the archive that
    /// `asked` did not name. With no volume asked, it reads no volume.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WrongLock`] when `lock` is not the lock of
    /// `archive`'s name, and the errors of the run files, docker and the
    /// archive's record. The steps before a failure stay done, and the
    /// archive stays retired.
    pub(crate) fn finish_held(
        &self,
        lock: &WorkspaceLock,
        archive: &mut Archive,
        asked: &VolumesAsked,
    ) -> Result<Option<Finish>, Error> {
        lock.check(&archive.name)?;
        let volumes = match archive
            .workspace_id
            .as_ref()
            .filter(|_| archive.containers_ran && !asked.volumes.is_empty())
        {
            Some(id) => {
                let volumes = self.runs.archived_volumes(id)?;
                if unnamed(&volumes, &asked.volumes) {
                    return Ok(None);
                }
                Some(volumes)
            }
            None => None,
        };
        self.finish_archive(archive, volumes).map(Some)
    }

    /// Deletes the run files of `archive`'s workspace and `volumes`, and
    /// marks `archive` finished, in that order, so the archive stays
    /// retired until the volumes are gone and a failure can be tried again.
    fn finish_archive(
        &self,
        archive: &mut Archive,
        volumes: Option<Volumes>,
    ) -> Result<Finish, Error> {
        let held = self
            .runs
            .forget(&archive.name, archive.workspace_id.as_ref())?;
        (self.workspaces.checkpoint)("forgotten")?;
        let mut names = Vec::new();
        if let Some(volumes) = volumes {
            names = volumes.names();
            if !volumes.is_empty() {
                self.runs.delete_volumes(volumes)?;
            }
        }
        (self.workspaces.checkpoint)("volumes")?;
        self.workspaces.finish_archive(self.archives, archive)?;
        Ok(Finish {
            volumes: names,
            held,
        })
    }

    /// Restores the archive that `confirmed` planned. It takes the lock of
    /// the archive's name and plans the restore again. A plan that names
    /// another archive than the confirmed one ends the run with
    /// [`Restoration::Changed`], and a name that a workspace holds again
    /// ends it with [`Restoration::Unchanged`], before anything is built.
    /// Then it rebuilds the workspace, writes its record and deletes the
    /// archive, and it holds the lock through the last write.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Busy`] when another command holds the lock for the
    /// whole lock wait, and the errors of the plan and of
    /// [`Workspaces::restore`]. A failure deletes what the run made and
    /// keeps the archive.
    pub fn restore(&self, confirmed: &RestorePlan) -> Result<Restoration, Error> {
        let name = &confirmed.archive.name;
        let _lock = self.runs.lock(name)?;
        let options = RestoreOptions::new(confirmed.force, confirmed.from_origin);
        match self.workspaces.plan_restore(self.archives, name, options)? {
            RestorePlanned::Ready(plan) if plan.archive.id == confirmed.archive.id => {
                match self.workspaces.restore(self.archives, *plan, self.runs)? {
                    RestoreOutcome::Restored(restored) => Ok(Restoration::Done(restored)),
                    RestoreOutcome::Refused(refusal) => Ok(Restoration::Refused(refusal)),
                }
            }
            RestorePlanned::Ready(_) => Ok(Restoration::Changed),
            RestorePlanned::Unchanged { .. } => Ok(Restoration::Unchanged),
            RestorePlanned::Refused(refusal) => Ok(Restoration::Refused(refusal)),
        }
    }

    /// Repairs the records of `name` as `confirmed` planned, which the
    /// caller got from [`Workspaces::repair`] in [`Mode::DryRun`] and asked
    /// about. It takes the locks of every name that the repair touches,
    /// each once and in name order: both names of a rename, so that a repair
    /// of either name excludes the other, and otherwise `name` alone. It
    /// plans the repair again, and changes nothing unless that plan is the
    /// confirmed one, which means the same directory moved aside, the same
    /// rename, or a deletion. A renamed archive is finished under the lock
    /// of its new name, as [`Lifecycle::finish`] finishes an archive.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Busy`] when another command holds a lock for the
    /// whole lock wait, and the errors of the plan, the repair and the
    /// finish. The steps before a failure stay done.
    pub fn repair(&self, name: &WorkspaceName, confirmed: &Repaired) -> Result<Repairing, Error> {
        let mut locks = Vec::new();
        for held in lock_names(name, confirmed) {
            locks.push(self.runs.lock(&held)?);
        }
        let again = self.workspaces.repair(self.archives, name, Mode::DryRun)?;
        if !same_repair(confirmed, &again) {
            return Ok(Repairing::Changed);
        }
        let repaired = self.workspaces.repair(self.archives, name, Mode::Change)?;
        let Repaired::Renamed { from, mut archive } = repaired else {
            return Ok(Repairing::Done(Box::new(RepairDone {
                repaired,
                finish: None,
            })));
        };
        // The second plan named the dated name, so its lock is held. A
        // repair whose rename chose another name under the locks has
        // renamed the archive and leaves it retired, which `mxv archive -w`
        // of the new name finishes.
        let Some(held) = locks.iter().find(|held| *held.name() == archive.name) else {
            return Ok(Repairing::Changed);
        };
        let finish = self.finish_held(held, &mut archive, &VolumesAsked::default())?;
        Ok(Repairing::Done(Box::new(RepairDone {
            repaired: Repaired::Renamed { from, archive },
            finish,
        })))
    }

    /// Returns the name of the live workspace that has the stable id of
    /// `archive`, because its docker volumes are then that workspace's. A
    /// workspace whose directories are partly gone counts too, so its
    /// volumes stay until `mxv remove` removes it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StateUnreadable`] when the workspace records cannot
    /// be read.
    pub fn volumes_in_use(&self, archive: &Archive) -> Result<Option<WorkspaceName>, Error> {
        let live = self.workspaces.store.list()?.workspaces;
        Ok(live
            .iter()
            .find(|workspace| rebuilt(archive, workspace) != Rebuilt::Unrelated)
            .map(|workspace| workspace.name.clone()))
    }
}

/// Returns the names whose locks the repair `planned` of `name` takes, in
/// the order it takes them. Two repairs that take the same two locks take
/// them in one order, so neither waits for the other while it holds the
/// first.
fn lock_names(name: &WorkspaceName, planned: &Repaired) -> Vec<WorkspaceName> {
    let mut names = vec![name.clone()];
    if let Repaired::Renamed { from, archive } = planned {
        names = vec![from.clone(), archive.name.clone()];
    }
    names.sort();
    names.dedup();
    names
}

/// Reports whether `again` is the repair that `confirmed` asked about: the
/// same directory moved aside, the same rename, a deletion of the same
/// archive, or the same archive's stopped restore undone. A deletion names its archive by id, because another repair,
/// a new archive and a stopped restore can leave another archive of the
/// name to delete while the question is open. Any other plan asks no
/// question, so it is never confirmed.
fn same_repair(confirmed: &Repaired, again: &Repaired) -> bool {
    match (confirmed, again) {
        (Repaired::MovedAside { kept: asked, .. }, Repaired::MovedAside { kept, .. }) => {
            asked == kept
        }
        (
            Repaired::Renamed {
                from: asked_from,
                archive: asked,
            },
            Repaired::Renamed { from, archive },
        ) => asked_from == from && asked.name == archive.name && asked.id == archive.id,
        (Repaired::Deleted(asked), Repaired::Deleted(deleted)) => {
            asked.archive.id == deleted.archive.id
        }
        (
            Repaired::Undone { archive: asked, .. },
            Repaired::Undone {
                archive: undone, ..
            },
        ) => asked.id == undone.id,
        _ => false,
    }
}

/// Reports whether `volumes` holds a volume that the question did not
/// name, which `named` lists.
fn unnamed(volumes: &Volumes, named: &[String]) -> bool {
    volumes.names().iter().any(|volume| !named.contains(volume))
}
