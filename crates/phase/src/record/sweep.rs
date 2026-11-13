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

use crate::clock::deadline_in;
use crate::engine::container::Identity as WorkspaceIdentity;
use crate::engine::host::{HostRunner, HostStop, HostStopped};
use crate::error::PhaseError;
use crate::name::WorkspaceId;
use crate::record::docker::{Docker, DockerError, DockerKind, DockerObject};
use crate::record::process::{self, Identity, Processes, vouched};
use crate::record::run::{
    Ended, EntryState, HostEntry, HostProcess, ObjectKey, RunRecord, VolumeLifetime,
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
    /// The process table, which tells what a recorded process is now.
    pub(crate) processes: &'a dyn Processes,
    /// The host runner, which stops a keeper and its process.
    pub(crate) host: &'a dyn HostRunner,
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
    let identity = WorkspaceIdentity {
        id: workspace.clone(),
        run_record: store.path(workspace),
    };
    if dry_run {
        let mut sweeper = Sweeper {
            identity: &identity,
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
        identity: &identity,
        journal: Some(&mut journal),
        reach,
        waits,
    };
    let swept = sweeper.run(&record)?;
    journal.finish()?;
    Ok(swept)
}

/// One sweep in progress.
struct Sweeper<'j, 's, 'a> {
    /// The workspace and the run record of the state directory being
    /// swept, whose labels a Docker object must carry.
    identity: &'a WorkspaceIdentity,
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
        let mut keepers = Vec::new();
        let mut processes = Vec::new();
        for (keeper, process) in record.host_pairs() {
            let (keeper, process) = self.host_pair(keeper, process)?;
            keepers.extend(keeper);
            processes.extend(process);
        }
        swept.items.extend(keepers);
        swept.items.extend(processes);
        for entry in &record.containers {
            let key = ObjectKey::Container(entry.app.clone(), entry.process.clone());
            let object = DockerObject {
                identity: self.identity,
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
        // A volume-owner helper that an interrupted `up` left behind is
        // not in the record, so the sweep finds it by its labels. A helper
        // matters only while the run it belongs to may still have a live
        // container or volume, so a record whose containers and volumes
        // have all stopped needs no Docker for it, and a host-only
        // workspace sweeps without Docker. When Docker does not answer,
        // there is nothing to remove: a helper cannot hold a volume while
        // Docker is down, and the next `up` removes a leftover helper
        // before it sets any owner.
        let live = record
            .containers
            .iter()
            .map(|entry| entry.state)
            .chain(record.volumes.iter().map(|entry| entry.state))
            .any(|state| state != EntryState::Stopped);
        if live && self.journal.is_some() {
            match self.reach.docker.remove_volume_helpers(self.identity) {
                Ok(()) | Err(DockerError::Unavailable { .. }) => {}
                Err(source) => return Err(PhaseError::Docker { source }),
            }
        }
        for entry in &record.networks {
            let key = ObjectKey::Network(entry.name.clone());
            let object = DockerObject {
                identity: self.identity,
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
                    identity: self.identity,
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

    /// Sweeps the keeper `keeper` and the host process `process` of one
    /// app process, either of which the record may lack. Each that phase
    /// can prove is its own is stopped through the host runner, which stops
    /// the keeper first so that it does not restart its process.
    fn host_pair(
        &mut self,
        keeper: Option<&HostEntry>,
        process: Option<&HostEntry>,
    ) -> Result<(Option<SweptItem>, Option<SweptItem>), PhaseError> {
        let keeper = keeper
            .map(|entry| self.look(Role::Keeper, entry, Identity::Gone))
            .transpose()?;
        // A keeper that is phase's own vouches for the group of its app
        // when the app's leader has gone.
        let voucher = keeper
            .as_ref()
            .and_then(|look| look.identity)
            .unwrap_or(Identity::Gone);
        let process = process
            .map(|entry| self.look(Role::Process, entry, voucher))
            .transpose()?;
        let (ours_keeper, ours_process) = (ours(keeper.as_ref()), ours(process.as_ref()));
        if (ours_keeper.is_some() || ours_process.is_some()) && self.journal.is_some() {
            for look in [&keeper, &process].into_iter().flatten() {
                if look.outcome.is_none() {
                    self.journal_step(|journal| journal.stopping(&look.key))?;
                }
            }
            let stop = HostStop {
                keeper: ours_keeper,
                app: ours_process,
                signal: crate::plan::restart::StopSignal::Term,
                grace: self.waits.kill,
                kill_wait: self.waits.kill,
            };
            let deadline = deadline_in(self.waits.kill.saturating_add(self.waits.kill));
            // The runner reports how the pair ended, and each process is
            // then looked at again, so each gets its own outcome.
            let _: HostStopped = self.reach.host.stop(&stop, deadline)?;
        }
        let keeper = keeper.map(|look| self.conclude(look)).transpose()?;
        let process = process.map(|look| self.conclude(look)).transpose()?;
        Ok((keeper, process))
    }

    /// Looks at the keeper or host process `entry` in `role`, and records
    /// it as stopped when nothing of it is left. `keeper` is what the
    /// process's keeper is, which vouches for the process's group when the
    /// keeper is phase's own. A group whose leader has gone and that
    /// nothing vouches for is unknown, and the sweep never signals it.
    fn look<'e>(
        &mut self,
        role: Role,
        entry: &'e HostEntry,
        keeper: Identity,
    ) -> Result<Look<'e>, PhaseError> {
        let key = match role {
            Role::Keeper => ObjectKey::Keeper(entry.app.clone(), entry.process.clone()),
            Role::Process => ObjectKey::Process(entry.app.clone(), entry.process.clone()),
        };
        let mut identity = None;
        let outcome = match &entry.host {
            _ if entry.state == EntryState::Stopped => Some(SweepOutcome::Gone),
            // phase never signals pid or group 0 or 1, so it cannot stop
            // such a process and leaves the entry as it is.
            Some(host) if !process::signallable(host) => Some(SweepOutcome::Unknown),
            None => {
                self.stopped(&key)?;
                Some(SweepOutcome::Gone)
            }
            Some(host) => match *identity.insert(vouched(keeper, self.identify(host)?)) {
                Identity::Gone => {
                    self.found_gone(&key, entry)?;
                    Some(SweepOutcome::Gone)
                }
                Identity::Reused => {
                    self.found_gone(&key, entry)?;
                    Some(SweepOutcome::PidReused)
                }
                Identity::Unknown | Identity::LeaderGone => Some(SweepOutcome::Unknown),
                Identity::Ours if self.journal.is_none() => Some(SweepOutcome::WouldStop),
                Identity::Ours => None,
            },
        };
        Ok(Look {
            role,
            entry,
            key,
            identity,
            outcome,
        })
    }

    /// Returns the item of `look`. A process that phase tried to stop is
    /// looked at again: it is stopped when nothing of it is left, and
    /// unstoppable otherwise.
    fn conclude(&mut self, look: Look<'_>) -> Result<SweptItem, PhaseError> {
        let handle = look
            .entry
            .host
            .as_ref()
            .map(|host| Handle::pid(host.pid, host.started_at_us));
        let outcome = match (look.outcome, &look.entry.host) {
            (Some(outcome), _) => outcome,
            (None, None) => SweepOutcome::Gone,
            (None, Some(host)) => match self.identify(host)? {
                Identity::Gone | Identity::Reused => {
                    self.stopped(&look.key)?;
                    SweepOutcome::Stopped
                }
                Identity::Unknown => SweepOutcome::Unknown,
                // The sweep found this process to be its own and tried to
                // stop it, so members left in its group after the leader
                // ended are its own too.
                Identity::Ours | Identity::LeaderGone => SweepOutcome::Unstoppable,
            },
        };
        Ok(SweptItem::new(
            look.entry.app.clone(),
            look.entry.process.clone(),
            look.role,
            handle,
            outcome,
        ))
    }

    /// Returns what the recorded process `host` is now. A dry run reaps
    /// nothing.
    fn identify(&self, host: &HostProcess) -> Result<Identity, PhaseError> {
        let processes = self.reach.processes;
        if self.journal.is_some() {
            processes.reap(host.pid);
        }
        process::identify(processes, host)
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

    /// Records that `key`, whose entry is `entry`, is gone, unless this is
    /// a dry run. A host process that the record names as running ended
    /// while nothing recorded how, so its entry records an end with no
    /// exit that no stop request caused, which replaces an earlier crash.
    fn found_gone(&mut self, key: &ObjectKey, entry: &HostEntry) -> Result<(), PhaseError> {
        if matches!(key, ObjectKey::Process(..)) && entry.state == EntryState::Running {
            return self.journal_step(|journal| journal.stopped_after(key, Ended::by_itself(None)));
        }
        self.stopped(key)
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

/// A keeper or host process that a sweep has looked at.
struct Look<'e> {
    /// Whether it is a keeper or a process.
    role: Role,
    /// Its record entry.
    entry: &'e HostEntry,
    /// Its key in the record.
    key: ObjectKey,
    /// What it was found to be, when the sweep read the process table.
    identity: Option<Identity>,
    /// Its outcome when the sweep leaves it alone, or `None` when it is
    /// running and phase stops it.
    outcome: Option<SweepOutcome>,
}

/// Returns the recorded process of `look` when the sweep stops it.
fn ours<'e>(look: Option<&Look<'e>>) -> Option<&'e HostProcess> {
    look.filter(|look| look.outcome.is_none())
        .and_then(|look| look.entry.host.as_ref())
}
