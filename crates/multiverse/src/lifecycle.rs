//! The order of a destructive command: lock the workspace's name, read its
//! records again, compare them with what the person confirmed, and only then
//! stop, delete or write.

use crate::error::Error;
use crate::runs::{DownOutcome, DownResult, Needed, Runs, Unstoppable};
use crate::workspaces::{Removal, RemovalOutcome, RemovalPlan, Workspaces};

#[cfg(test)]
mod tests;

/// The destructive commands of one workspace store, each of which holds the
/// workspace's lock from its last check to its last write.
#[derive(Debug)]
pub struct Lifecycle<'a, B> {
    /// The workspaces and their records.
    workspaces: &'a Workspaces<B>,
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
#[non_exhaustive]
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

impl<'a, B> Lifecycle<'a, B> {
    /// Returns the destructive commands of `workspaces`, whose run files,
    /// volumes and locks `runs` holds.
    #[must_use]
    pub const fn new(workspaces: &'a Workspaces<B>, runs: &'a Runs) -> Self {
        Self { workspaces, runs }
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
}
