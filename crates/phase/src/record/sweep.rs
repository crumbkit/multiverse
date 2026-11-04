//! `sweep`: stops what a workspace's run record names and phase no longer
//! supervises, and removes the record once everything is stopped.
//!
//! `sweep` stops keepers first, so that no keeper restarts its process,
//! then host processes, containers, networks and volumes. It stops a host
//! process only when its pid and start time both match the record, and on
//! Linux only when the process started in the current boot. It sends
//! `TERM`, to the process's group when the process leads its group and to
//! the process alone otherwise, and waits up to the kill wait for the
//! process and its group to go. Then it sends `KILL` and waits again. A
//! process that is still there after that is unstoppable, and the record
//! keeps it. A process whose start time phase may not read, as inside a
//! sandbox, is unknown, and the record keeps it unchanged. So is a process
//! recorded without a boot id on a system whose start times count from the
//! boot, such as Linux. A Docker object
//! is removed through [`Docker`], and when Docker does not answer, `sweep`
//! fails and keeps the record.

use std::time::Duration;

use rustix::process::Signal;

use crate::clock::{POLL_INTERVAL, now, pause};
use crate::error::PhaseError;
use crate::name::WorkspaceId;
use crate::record::docker::{Docker, DockerKind, DockerObject};
use crate::record::process::{Processes, Start};
use crate::record::run::{
    EntryState, HostEntry, HostProcess, ObjectKey, RunRecord, VolumeLifetime,
};
use crate::record::store::{Journal, RunStore};
use crate::record::{
    ContainerId, Handle, ObjectKind, Role, SweepOutcome, Swept, SweptItem, SweptObject,
};

#[cfg(test)]
mod tests;

/// The waits of one sweep.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Waits {
    /// How long `sweep` waits for the lifecycle lock.
    pub(crate) lock: Duration,
    /// How long `sweep` waits after `TERM`, and again after `KILL`.
    pub(crate) kill: Duration,
}

/// What `sweep` calls to find and stop the objects of a record.
#[derive(Clone, Copy)]
pub(crate) struct Reach<'a> {
    /// The Docker calls.
    pub(crate) docker: &'a dyn Docker,
    /// The process table.
    pub(crate) processes: &'a dyn Processes,
}

/// Sweeps the workspace `workspace`, as the module docs say. A workspace
/// with no record has nothing to sweep, and `sweep` takes no lock for it.
/// With `dry_run`, it reads the record without the lock, reports what it
/// would stop, and changes nothing, not even by reaping a child.
///
/// # Errors
///
/// Returns [`PhaseError::Busy`] when another operation holds the lock,
/// the errors of reading and writing the record, [`PhaseError::Docker`]
/// when a Docker call fails, and [`PhaseError::ProcessTable`] when phase
/// cannot read or signal processes.
pub(crate) fn sweep(
    store: &RunStore,
    workspace: &WorkspaceId,
    dry_run: bool,
    reach: Reach<'_>,
    waits: Waits,
) -> Result<Swept, PhaseError> {
    let Some(record) = store.read(workspace)? else {
        return Ok(Swept::default());
    };
    if dry_run {
        let mut sweeper = Sweeper {
            workspace,
            journal: None,
            reach,
            waits,
        };
        return sweeper.run(&record);
    }
    let mut journal = Journal::open(store, workspace, waits.lock)?;
    let Some(record) = journal.record().cloned() else {
        return Ok(Swept::default());
    };
    let mut sweeper = Sweeper {
        workspace,
        journal: Some(&mut journal),
        reach,
        waits,
    };
    let swept = sweeper.run(&record)?;
    journal.finish()?;
    Ok(swept)
}

/// What a recorded host process is now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Identity {
    /// The recorded process, or a member of its group, is running.
    Ours,
    /// Another process has the pid.
    Reused,
    /// phase may not read the process that has the pid, so it cannot tell
    /// whether it is the recorded one.
    Unknown,
    /// Nothing of the recorded process is left.
    Gone,
}

/// One sweep in progress.
struct Sweeper<'j, 's, 'a> {
    /// The workspace, whose label a Docker object found by name carries.
    workspace: &'a WorkspaceId,
    /// The journal that records each step, or `None` for a dry run.
    journal: Option<&'j mut Journal<'s>>,
    /// The Docker calls and the process table.
    reach: Reach<'a>,
    /// The waits.
    waits: Waits,
}

impl Sweeper<'_, '_, '_> {
    /// Sweeps every object of `record`.
    fn run(&mut self, record: &RunRecord) -> Result<Swept, PhaseError> {
        let mut swept = Swept::default();
        for (role, entries) in [
            (Role::Keeper, &record.keepers),
            (Role::Process, &record.processes),
        ] {
            for entry in entries {
                swept.items.push(self.host(role, entry)?);
            }
        }
        for entry in &record.containers {
            let key = ObjectKey::Container(entry.app.clone(), entry.process.clone());
            let object = DockerObject {
                workspace: self.workspace,
                kind: DockerKind::Container,
                name: &entry.name,
                id: entry.id.as_ref().map(ContainerId::as_str),
            };
            let (outcome, found) = self.docker_object(&key, entry.state, object)?;
            let handle = entry
                .id
                .clone()
                .or_else(|| found.and_then(|id| ContainerId::try_from(id).ok()))
                .map(Handle::container);
            swept.items.push(SweptItem::new(
                entry.app.clone(),
                entry.process.clone(),
                Role::Process,
                handle,
                outcome,
            ));
        }
        for entry in &record.networks {
            let key = ObjectKey::Network(entry.name.clone());
            let object = DockerObject {
                workspace: self.workspace,
                kind: DockerKind::Network,
                name: &entry.name,
                id: entry.id.as_ref().map(|id| id.as_str()),
            };
            let (outcome, _) = self.docker_object(&key, entry.state, object)?;
            swept.objects.push(SweptObject::new(
                ObjectKind::Network,
                entry.name.clone(),
                outcome,
            ));
        }
        for entry in &record.volumes {
            let outcome = if entry.lifetime == VolumeLifetime::Workspace {
                SweepOutcome::Kept
            } else {
                let key = ObjectKey::Volume(entry.name.clone());
                let object = DockerObject {
                    workspace: self.workspace,
                    kind: DockerKind::Volume,
                    name: &entry.name,
                    id: None,
                };
                self.docker_object(&key, entry.state, object)?.0
            };
            swept.objects.push(SweptObject::new(
                ObjectKind::Volume,
                entry.name.clone(),
                outcome,
            ));
        }
        Ok(swept)
    }

    /// Sweeps the keeper or host process `entry`.
    fn host(&mut self, role: Role, entry: &HostEntry) -> Result<SweptItem, PhaseError> {
        let key = match role {
            Role::Keeper => ObjectKey::Keeper(entry.app.clone(), entry.process.clone()),
            Role::Process => ObjectKey::Process(entry.app.clone(), entry.process.clone()),
        };
        let handle = entry
            .host
            .as_ref()
            .map(|host| Handle::pid(host.pid, host.started_at_us));
        let outcome = match &entry.host {
            _ if entry.state == EntryState::Stopped => SweepOutcome::Gone,
            None => {
                self.stopped(&key)?;
                SweepOutcome::Gone
            }
            Some(host) => match self.identify(host)? {
                Identity::Gone => {
                    self.stopped(&key)?;
                    SweepOutcome::Gone
                }
                Identity::Reused => {
                    self.stopped(&key)?;
                    SweepOutcome::PidReused
                }
                Identity::Unknown => SweepOutcome::Unknown,
                Identity::Ours => self.stop_host(&key, host)?,
            },
        };
        Ok(SweptItem::new(
            entry.app.clone(),
            entry.process.clone(),
            role,
            handle,
            outcome,
        ))
    }

    /// Stops the running host process `host`, whose object is `key`.
    fn stop_host(
        &mut self,
        key: &ObjectKey,
        host: &HostProcess,
    ) -> Result<SweepOutcome, PhaseError> {
        if self.journal.is_none() {
            return Ok(SweepOutcome::WouldStop);
        }
        self.journal_step(|journal| journal.stopping(key))?;
        for signal in [Signal::TERM, Signal::KILL] {
            self.reach.processes.signal(host, signal)?;
            if self.wait_until_gone(host)? {
                self.journal_step(|journal| journal.stopped(key))?;
                return Ok(SweepOutcome::Stopped);
            }
        }
        Ok(SweepOutcome::Unstoppable)
    }

    /// Returns what the recorded process `host` is now. A process whose
    /// pid has no process may have left members in its group, and those
    /// are still ours, because the system never gives a new process the id
    /// of a group that has members. A dry run reaps nothing.
    fn identify(&self, host: &HostProcess) -> Result<Identity, PhaseError> {
        let processes = self.reach.processes;
        if self.journal.is_some() {
            processes.reap(host.pid);
        }
        // A start time counts from the boot on a system that has a boot id,
        // so a process recorded there without one cannot be matched, and
        // phase leaves it alone.
        match (&host.boot_id, processes.boot_id()?) {
            (Some(recorded), current) if current.as_ref() != Some(recorded) => {
                return Ok(Identity::Gone);
            }
            (None, Some(_)) => return Ok(Identity::Unknown),
            _ => {}
        }
        Ok(match processes.start_time(host.pid)? {
            Start::At(started) if started == host.started_at_us => Identity::Ours,
            Start::At(_) => Identity::Reused,
            Start::Unreadable => Identity::Unknown,
            Start::Absent if host.group == host.pid && processes.group_exists(host.group) => {
                Identity::Ours
            }
            Start::Absent => Identity::Gone,
        })
    }

    /// Polls `host` until nothing of it is left or the kill wait passes,
    /// and reports whether it went.
    fn wait_until_gone(&self, host: &HostProcess) -> Result<bool, PhaseError> {
        let deadline = now() + self.waits.kill;
        // A process that phase did not start gives phase no event when it
        // ends, so `sweep` polls the process table (BASE-11).
        loop {
            if matches!(self.identify(host)?, Identity::Gone | Identity::Reused) {
                return Ok(true);
            }
            if now() >= deadline {
                return Ok(false);
            }
            pause(POLL_INTERVAL);
        }
    }

    /// Sweeps the Docker object `object`, whose record entry is `key` in
    /// `state`, and returns the outcome and the id that Docker gave.
    fn docker_object(
        &mut self,
        key: &ObjectKey,
        state: EntryState,
        object: DockerObject<'_>,
    ) -> Result<(SweepOutcome, Option<String>), PhaseError> {
        if state == EntryState::Stopped {
            return Ok((SweepOutcome::Gone, None));
        }
        let docker = self.reach.docker;
        let docker_error = |source| PhaseError::Docker { source };
        let Some(id) = docker.find(&object).map_err(docker_error)? else {
            self.stopped(key)?;
            return Ok((SweepOutcome::Gone, None));
        };
        if self.journal.is_none() {
            return Ok((SweepOutcome::WouldStop, Some(id)));
        }
        self.journal_step(|journal| journal.stopping(key))?;
        docker
            .remove(object.kind, &id, self.waits.kill)
            .map_err(docker_error)?;
        let removed = DockerObject {
            id: Some(&id),
            ..object
        };
        if docker.find(&removed).map_err(docker_error)?.is_some() {
            return Ok((SweepOutcome::Unstoppable, Some(id)));
        }
        self.journal_step(|journal| journal.stopped(key))?;
        Ok((SweepOutcome::Stopped, Some(id)))
    }

    /// Records that `key` is gone, unless this is a dry run.
    fn stopped(&mut self, key: &ObjectKey) -> Result<(), PhaseError> {
        self.journal_step(|journal| journal.stopped(key))
    }

    /// Runs `step` on the journal, unless this is a dry run.
    fn journal_step(
        &mut self,
        step: impl FnOnce(&mut Journal<'_>) -> Result<(), PhaseError>,
    ) -> Result<(), PhaseError> {
        match self.journal.as_deref_mut() {
            Some(journal) => step(journal),
            None => Ok(()),
        }
    }
}
