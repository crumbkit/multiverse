//! The order of a destructive command: lock the workspace's name, read its
//! records again, compare them with what the person confirmed, and only then
//! stop, delete or write.

use crate::archive::{Archive, ArchiveState, ArchivedRemoval, Archives};
use crate::error::Error;
use crate::health::{Rebuilt, rebuilt};
use crate::name::WorkspaceName;
use crate::runs::{DownOutcome, DownResult, Needed, Runs, Unstoppable};
use crate::store::Lock;
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

/// The docker volumes that the question of an archive's deletion named,
/// which the deletion may delete.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VolumesAsked {
    /// The volumes' docker names, or none when the question named none.
    volumes: Vec<String>,
}

impl VolumesAsked {
    /// Returns what a question that named the docker volumes `volumes`
    /// allowed. With none, the deletion reads and deletes no volume.
    #[must_use]
    pub const fn new(volumes: Vec<String>) -> Self {
        Self { volumes }
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

impl<'a, B> Lifecycle<'a, B> {
    /// Returns the destructive commands of `workspaces` and `archives`,
    /// whose run files, volumes and locks `runs` holds.
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
        }
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

    /// Deletes the archive that `confirmed` is, for a caller that holds the
    /// lock of its name as `_lock`, because a second lock of one name in one
    /// process waits and fails with [`Error::Busy`]. It reads the archive of
    /// the name again, and deletes nothing unless that is `confirmed`'s
    /// archive, with the same id, and finished.
    ///
    /// When `asked` names docker volumes, it refuses while a live workspace
    /// has the archive's stable id, and refuses when docker holds a volume of
    /// the archive that `asked` did not name. Then it deletes those volumes,
    /// the pins and the record, in that order, so the record that holds the
    /// volumes' stable id stays until they are gone, and a failure can be
    /// tried again.
    ///
    /// # Errors
    ///
    /// Returns the errors of the archive's record, of docker, and of
    /// [`Workspaces::remove_archive`]. The steps before a failure stay done.
    pub fn delete_archive_held(
        &self,
        _lock: &Lock,
        confirmed: &Archive,
        asked: &VolumesAsked,
    ) -> Result<ArchiveDeleted, Error> {
        let Some(archive) = self.archives.read(&confirmed.name)? else {
            return Ok(ArchiveDeleted::Unchanged);
        };
        if archive.id != confirmed.id || archive.state != ArchiveState::Archived {
            return Ok(ArchiveDeleted::Changed);
        }
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
        let removed = self
            .workspaces
            .remove_archive(self.archives, archive, Mode::Change)?;
        Ok(ArchiveDeleted::Done(Box::new(removed)))
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
