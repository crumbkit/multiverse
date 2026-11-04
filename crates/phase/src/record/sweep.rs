//! `sweep`: stops what a workspace's run record names and phase no longer
//! supervises, and removes the record once everything is stopped.
//!
//! `sweep` stops keepers first, so that no keeper restarts its process,
//! then host processes, containers, networks and volumes. It stops a host
//! process only when its pid and start time both match the record. It
//! sends `TERM` to the process's group, waits up to the kill wait for the
//! process and its group to go, then sends `KILL` and waits again. A
//! process that is still there after that is unstoppable, and the record
//! keeps it. A Docker object is removed through [`Docker`], and when Docker
//! does not answer, `sweep` fails and keeps the record.

use std::time::{Duration, Instant};

use rustix::process::Signal;

use crate::clock::{POLL_INTERVAL, pause};
use crate::error::PhaseError;
use crate::name::WorkspaceId;
use crate::record::docker::{Docker, DockerKind, DockerObject};
use crate::record::process::{self, Start};
use crate::record::run::{EntryState, HostEntry, HostProcess, ObjectKey, VolumeLifetime};
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

/// Sweeps the workspace `workspace`, as the module docs say. With
/// `dry_run`, it reads the record without the lock, reports what it would
/// stop, and changes nothing.
///
/// # Errors
///
/// Returns [`PhaseError::Busy`] when another operation holds the lock,
/// the errors of reading and writing the record, [`PhaseError::Docker`]
/// when Docker does not answer, and [`PhaseError::ProcessTable`] when the
/// process table cannot be read.
pub(crate) fn sweep(
    store: &RunStore,
    workspace: &WorkspaceId,
    dry_run: bool,
    docker: &dyn Docker,
    waits: Waits,
) -> Result<Swept, PhaseError> {
    if dry_run {
        let Some(record) = store.read(workspace)? else {
            return Ok(Swept::default());
        };
        let mut sweeper = Sweeper {
            journal: None,
            docker,
            waits,
        };
        return sweeper.run(&record);
    }
    let mut journal = Journal::open(store, workspace, waits.lock)?;
    let Some(record) = journal.record().cloned() else {
        return Ok(Swept::default());
    };
    let mut sweeper = Sweeper {
        journal: Some(&mut journal),
        docker,
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
    /// Nothing of the recorded process is left.
    Gone,
}

/// One sweep in progress.
struct Sweeper<'j, 's, 'd> {
    /// The journal that records each step, or `None` for a dry run.
    journal: Option<&'j mut Journal<'s>>,
    /// The Docker calls.
    docker: &'d dyn Docker,
    /// The waits.
    waits: Waits,
}

impl Sweeper<'_, '_, '_> {
    /// Sweeps every object of `record`.
    fn run(&mut self, record: &crate::record::run::RunRecord) -> Result<Swept, PhaseError> {
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
            let recorded = entry.id.as_ref().map(ContainerId::as_str);
            let (outcome, found) = self.docker_object(
                &key,
                entry.state,
                DockerObject {
                    kind: DockerKind::Container,
                    name: &entry.name,
                    id: recorded,
                },
            )?;
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
            .map(|host| Handle::pid(host.pid, host.started_at_us));
        let outcome = match entry.host {
            _ if entry.state == EntryState::Stopped => SweepOutcome::Gone,
            None => {
                self.stopped(&key)?;
                SweepOutcome::Gone
            }
            Some(host) => match identify(host)? {
                Identity::Gone => {
                    self.stopped(&key)?;
                    SweepOutcome::Gone
                }
                Identity::Reused => {
                    self.stopped(&key)?;
                    SweepOutcome::PidReused
                }
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
        host: HostProcess,
    ) -> Result<SweepOutcome, PhaseError> {
        let Some(journal) = self.journal.as_deref_mut() else {
            return Ok(SweepOutcome::WouldStop);
        };
        journal.stopping(key)?;
        for signal in [Signal::TERM, Signal::KILL] {
            process::signal(host.pid, host.group, signal)?;
            if wait_until_gone(host, self.waits.kill)? {
                journal.stopped(key)?;
                return Ok(SweepOutcome::Stopped);
            }
        }
        Ok(SweepOutcome::Unstoppable)
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
        let docker_error = |source| PhaseError::Docker { source };
        let Some(id) = self.docker.find(&object).map_err(docker_error)? else {
            self.stopped(key)?;
            return Ok((SweepOutcome::Gone, None));
        };
        let Some(journal) = self.journal.as_deref_mut() else {
            return Ok((SweepOutcome::WouldStop, Some(id)));
        };
        journal.stopping(key)?;
        self.docker
            .remove(object.kind, &id, self.waits.kill)
            .map_err(docker_error)?;
        let removed = DockerObject {
            id: Some(&id),
            ..object
        };
        if self.docker.find(&removed).map_err(docker_error)?.is_some() {
            return Ok((SweepOutcome::Unstoppable, Some(id)));
        }
        journal.stopped(key)?;
        Ok((SweepOutcome::Stopped, Some(id)))
    }

    /// Records that `key` is gone, unless this is a dry run.
    fn stopped(&mut self, key: &ObjectKey) -> Result<(), PhaseError> {
        match self.journal.as_deref_mut() {
            Some(journal) => journal.stopped(key),
            None => Ok(()),
        }
    }
}

/// Returns what the recorded process `host` is now. A process whose pid has
/// no process may have left members in its group, and those are still
/// ours, because the system never gives a new process the id of a group
/// that has members.
fn identify(host: HostProcess) -> Result<Identity, PhaseError> {
    process::reap(host.pid);
    Ok(match process::start_time(host.pid)? {
        Start::At(started) if started == host.started_at_us => Identity::Ours,
        Start::At(_) | Start::Unreadable => Identity::Reused,
        Start::Absent if host.group == host.pid && process::group_exists(host.group) => {
            Identity::Ours
        }
        Start::Absent => Identity::Gone,
    })
}

/// Polls `host` until nothing of it is left or `wait` passes, and reports
/// whether it went.
fn wait_until_gone(host: HostProcess, wait: Duration) -> Result<bool, PhaseError> {
    let deadline = Instant::now() + wait;
    loop {
        if identify(host)? != Identity::Ours {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        pause(POLL_INTERVAL);
    }
}
