//! Building a workspace's [`Status`] from its run record, the process
//! table and Docker, as the status module's docs say.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;

use crate::engine::container::ContainerState;
use crate::engine::restart::{AfterExit, after_exit};
use crate::error::PhaseError;
use crate::logs::log_path;
use crate::name::{AppName, ProcessName, WorkspaceId};
use crate::plan::restart::Restart;
use crate::plan::{AppPlan, Plan, ProcessPlan, RunnerKind};
use crate::record::Handle;
use crate::record::docker::{Docker, DockerKind, DockerObject, SeenContainer};
use crate::record::process::{self, Identity, Processes, vouched};
use crate::record::run::{ContainerEntry, Ended, EntryState, Exit, HostEntry, RunRecord};
use crate::record::store::RunStore;
use crate::sandbox;
use crate::status::{
    LastExit, ProcessState, SourceSummary, Status, StatusRow, UnknownReason, WorkspaceState,
};

/// What `status` reads besides the run record.
#[derive(Clone, Copy)]
pub(crate) struct Sources<'a> {
    /// The directory that holds phase's state, under which the logs live.
    pub(crate) state_dir: &'a Path,
    /// The process table.
    pub(crate) processes: &'a dyn Processes,
    /// Docker.
    pub(crate) docker: &'a dyn Docker,
}

/// Returns the status of the workspace whose stable id is `workspace`.
///
/// With a `plan` of that workspace, `status` gives one row per planned
/// process in the plan's order, with the source, runner and address that
/// the plan gives, and then a row for each live record entry that the plan
/// lacks. Without one, or with a plan of another workspace, it gives a row
/// for each record entry that is not stopped, and leaves empty the fields
/// that only a plan gives. It takes no lock.
///
/// # Errors
///
/// Returns [`PhaseError::RecordUnreadable`] or [`PhaseError::NewerRecord`]
/// when the record cannot be used, and [`PhaseError::ProcessTable`] when
/// the process table cannot be read.
pub(crate) fn status(
    store: &RunStore,
    workspace: &WorkspaceId,
    plan: Option<&Plan>,
    sources: Sources<'_>,
) -> Result<Status, PhaseError> {
    let plan = plan.filter(|plan| plan.id() == workspace);
    let record = store.read(workspace)?;
    let mut rows = Vec::new();
    if let Some(plan) = plan {
        for app in plan.apps() {
            for planned in &app.processes {
                rows.push(planned_row(
                    workspace,
                    record.as_ref(),
                    app,
                    planned,
                    sources,
                )?);
            }
        }
    }
    if let Some(record) = &record {
        let planned = |app: &AppName, process: &ProcessName| {
            plan.is_some_and(|plan| {
                plan.apps().iter().any(|found| {
                    &found.name == app && found.processes.iter().any(|named| &named.name == process)
                })
            })
        };
        for (keeper, entry) in record.host_pairs() {
            let Some(named) = keeper.or(entry) else {
                continue;
            };
            // An entry that records how its process ended by itself shows
            // as exited, so it stays among the rows.
            let exited =
                entry.is_some_and(|entry| entry.ended.is_some_and(|ended| !ended.requested));
            let live = exited || [keeper, entry].into_iter().flatten().any(HostEntry::live);
            if live && !planned(&named.app, &named.process) {
                let mut row = host_row(
                    workspace,
                    keeper,
                    entry,
                    &named.app,
                    &named.process,
                    sources,
                )?;
                enrich(
                    &mut row,
                    plan.and_then(|plan| app_of(plan, &named.app)),
                    None,
                );
                rows.push(row);
            }
        }
        for entry in &record.containers {
            if entry.state != EntryState::Stopped && !planned(&entry.app, &entry.process) {
                let mut row = container_row(workspace, entry, sources);
                enrich(
                    &mut row,
                    plan.and_then(|plan| app_of(plan, &entry.app)),
                    None,
                );
                rows.push(row);
            }
        }
    }
    let mut status = Status::new(workspace.clone(), workspace_state(&rows));
    status.name = plan.map(|plan| plan.workspace().clone());
    status.rows = rows;
    Ok(status)
}

/// Returns the app `name` of `plan`, or `None`.
fn app_of<'p>(plan: &'p Plan, name: &AppName) -> Option<&'p AppPlan> {
    plan.apps().iter().find(|app| &app.name == name)
}

/// Returns the row of the planned process `planned` of `app`, from its
/// record entries when `record` names it.
fn planned_row(
    workspace: &WorkspaceId,
    record: Option<&RunRecord>,
    app: &AppPlan,
    planned: &ProcessPlan,
    sources: Sources<'_>,
) -> Result<StatusRow, PhaseError> {
    let container = record.and_then(|record| {
        record
            .containers
            .iter()
            .find(|entry| entry.app == app.name && entry.process == planned.name)
    });
    let mut row = match (app.runner, container) {
        (RunnerKind::Container, Some(entry)) => container_row(workspace, entry, sources),
        (RunnerKind::Container, None) => StatusRow::new(
            app.name.clone(),
            planned.name.clone(),
            RunnerKind::Container,
            None,
            ProcessState::Stopped,
        ),
        (RunnerKind::Host, _) => {
            let (keeper, entry) = record.map_or((None, None), |record| {
                record.host_pair(&app.name, &planned.name)
            });
            host_row(workspace, keeper, entry, &app.name, &planned.name, sources)?
        }
    };
    enrich(&mut row, Some(app), Some(planned));
    Ok(row)
}

/// Fills in what the plan gives a row: the source and runner of its `app`,
/// and the address of the port that `planned` listens on when the record
/// named none.
fn enrich(row: &mut StatusRow, app: Option<&AppPlan>, planned: Option<&ProcessPlan>) {
    let Some(app) = app else {
        return;
    };
    row.source = Some(SourceSummary::from(&app.source));
    row.runner = app.runner;
    // A row that its record filled shows how phase launched the process,
    // and any other row shows how phase would launch it now.
    if row.sandbox.is_none() {
        row.sandbox = sandbox::sandboxing(app, &sandbox::platform());
    }
    if row.address.is_none() && app.runner == RunnerKind::Host {
        let port = planned
            .and_then(|planned| planned.port.as_ref())
            .and_then(|port| app.ports.get(port));
        row.address = port.map(|port| loopback(port.get()));
    }
}

/// Returns the address of `port` on the IPv4 loopback address.
const fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

/// Returns the row of the host process `process` of `app`, whose keeper and
/// own entries are `keeper` and `entry`.
fn host_row(
    workspace: &WorkspaceId,
    keeper: Option<&HostEntry>,
    entry: Option<&HostEntry>,
    app: &AppName,
    process: &ProcessName,
    sources: Sources<'_>,
) -> Result<StatusRow, PhaseError> {
    let state = match host_state(keeper, entry, sources.processes)? {
        ProcessState::Running if entry.is_some_and(observed_ready) => ProcessState::Ready,
        state => state,
    };
    let mut row = StatusRow::new(app.clone(), process.clone(), RunnerKind::Host, None, state);
    if let Some(entry) = entry {
        row.restarts = entry.restarts;
        row.last_exit = entry.ended.filter(|ended| !ended.requested).map(|ended| {
            let exit = ended.exit;
            LastExit::new(exit.and_then(Exit::code), exit.and_then(Exit::signal))
        });
        row.address = entry.listen.as_ref().map(|listen| loopback(listen.port));
        row.sandbox.clone_from(&entry.sandbox);
        if let Some(host) = entry
            .host
            .as_ref()
            .filter(|_| entry.live() && state.may_run())
        {
            row.handle = Some(Handle::pid(host.pid, host.started_at_us));
            row.started_at_ms = process::started_at_ms(host.started_at_us);
        }
    }
    row.log = Some(log_path(sources.state_dir, workspace, app, process));
    Ok(row)
}

/// Reports whether phase observed the host process of `entry` ready since
/// its keeper last restarted it.
fn observed_ready(entry: &HostEntry) -> bool {
    entry
        .ready
        .as_ref()
        .is_some_and(|ready| ready.restarts == entry.restarts)
}

/// Returns what a host process is doing, from its keeper and own entries
/// and what the process table shows of each live one. A stopped entry was
/// checked as gone when it was recorded, so only live entries are looked
/// up.
///
/// # Errors
///
/// Returns [`PhaseError::ProcessTable`] when the table cannot be read.
pub(crate) fn host_state(
    keeper: Option<&HostEntry>,
    entry: Option<&HostEntry>,
    processes: &dyn Processes,
) -> Result<ProcessState, PhaseError> {
    let mut identities = [Identity::Gone, Identity::Gone];
    for (identity, named) in identities.iter_mut().zip([keeper, entry]) {
        let Some(host) = named
            .filter(|named| named.live())
            .and_then(|named| named.host.as_ref())
        else {
            continue;
        };
        if !process::signallable(host) {
            return Ok(ProcessState::unknown(UnknownReason::Unsignallable));
        }
        *identity = process::identify(processes, host)?;
    }
    identities[1] = vouched(identities[0], identities[1]);
    if identities.contains(&Identity::Unknown) {
        return Ok(ProcessState::unknown(UnknownReason::Unreadable));
    }
    if identities.contains(&Identity::LeaderGone) {
        return Ok(ProcessState::unknown(UnknownReason::LeaderGone));
    }
    let [keeper_ours, process_ours] = identities.map(|identity| identity == Identity::Ours);
    let stopping = [keeper, entry]
        .into_iter()
        .flatten()
        .any(|named| named.state == EntryState::Stopping);
    Ok(if stopping && (keeper_ours || process_ours) {
        ProcessState::Stopping
    } else if process_ours {
        ProcessState::Running
    } else if keeper_ours {
        let started =
            entry.is_some_and(|entry| entry.host.is_some() && entry.state != EntryState::Planned);
        match entry.filter(|_| started) {
            Some(entry) => after_end(entry),
            None => ProcessState::Starting,
        }
    } else if let Some(ended) = entry
        .filter(|entry| matches!(entry.state, EntryState::Stopping | EntryState::Stopped))
        .and_then(|entry| entry.ended)
        .filter(|ended| !ended.requested)
    {
        // The keeper recorded that its app ended by itself and did not
        // restart it, or something killed the keeper while it stopped what
        // the app left.
        exited(ended)
    } else if entry.is_some_and(|entry| {
        entry.host.is_some()
            && (entry.state == EntryState::Running
                || (entry.state == EntryState::Stopping && entry.ended.is_none()))
    }) {
        // The record names the process as running, or as stopping with no
        // end, and neither it nor its keeper is there: the keeper ended
        // before it recorded the end, as when something killed it. An end
        // that a running entry holds is an earlier crash's, which the
        // keeper restarted from.
        ProcessState::unrecorded()
    } else {
        ProcessState::Stopped
    })
}

/// Returns what a started host process whose keeper runs and whose app
/// has gone is doing, from its `entry`. The keeper restarts the app or
/// stops what is left of its group, as the restart policy that the entry
/// records says. Before the keeper records the end, the entry is still
/// `running`, and the process is restarting only when the policy restarts
/// it however it ended. A record without a policy, which an older phase
/// wrote, reads as restarting.
fn after_end(entry: &HostEntry) -> ProcessState {
    let restarts = |restart: &Restart, failed: bool| {
        matches!(
            after_exit(restart, failed, entry.restarts),
            AfterExit::Restart { .. }
        )
    };
    let ended = entry.ended.filter(|_| entry.state != EntryState::Running);
    match (&entry.restart, ended) {
        (_, Some(ended)) if ended.requested => ProcessState::Stopping,
        (None, _) => ProcessState::Restarting,
        (Some(restart), None) if restarts(restart, false) => ProcessState::Restarting,
        (Some(_), None) => ProcessState::Stopping,
        (Some(restart), Some(ended)) if restarts(restart, ended.exit != Some(Exit::Code(0))) => {
            ProcessState::Restarting
        }
        (Some(_), Some(ended)) => exited(ended),
    }
}

/// Returns the state of a process that ended by itself as `ended`.
const fn exited(ended: Ended) -> ProcessState {
    match ended.exit {
        Some(Exit::Signal(signal)) => ProcessState::killed(signal),
        Some(Exit::Code(code)) => ProcessState::exited(Some(code)),
        None => ProcessState::unrecorded(),
    }
}

/// Returns the row of the container `entry`, whose state Docker gives. A
/// Docker that does not answer makes the state unknown, never stopped.
fn container_row(
    workspace: &WorkspaceId,
    entry: &ContainerEntry,
    sources: Sources<'_>,
) -> StatusRow {
    let seen = if entry.state == EntryState::Stopped {
        Ok(None)
    } else {
        sources.docker.container_seen(&DockerObject {
            workspace,
            kind: DockerKind::Container,
            name: &entry.name,
            id: entry.id.as_ref().map(crate::record::ContainerId::as_str),
        })
    };
    let planned = entry.state == EntryState::Planned;
    let state = match seen {
        Ok(Some(seen)) if seen.state == ContainerState::Running && observed(entry, &seen) => {
            ProcessState::Ready
        }
        Ok(Some(seen)) => container_state(seen.state, planned),
        Ok(None) => container_state(ContainerState::Created, planned),
        Err(_) => ProcessState::unknown(UnknownReason::DockerUnavailable),
    };
    let mut row = StatusRow::new(
        entry.app.clone(),
        entry.process.clone(),
        RunnerKind::Container,
        None,
        state,
    );
    if state.may_run() {
        row.handle = entry.id.clone().map(Handle::container);
    }
    row
}

/// Reports whether phase observed the container of `entry` ready in the
/// start that Docker reports now: the start that the readiness records is
/// the one Docker printed. Every start changes Docker's start time,
/// including a restart under the restart policy, `docker restart`, a
/// `docker start` after a stop and a restart of the daemon, so any of them
/// makes the container not ready. A start time that Docker did not print,
/// or readiness that records none, counts as not ready.
fn observed(entry: &ContainerEntry, seen: &SeenContainer) -> bool {
    entry.ready.as_ref().is_some_and(|ready| {
        ready.container_started_at.is_some() && ready.container_started_at == seen.started_at
    })
}

/// Returns what a container in Docker's `state` is doing, when phase has
/// not observed it ready since Docker last started it. A container that
/// Docker does not have reads as [`ContainerState::Created`]. A container
/// that the record plans, which `planned` reports, and that Docker has not
/// started yet is starting, as a host process whose keeper has not started
/// it is.
const fn container_state(state: ContainerState, planned: bool) -> ProcessState {
    match state {
        ContainerState::Running => ProcessState::Running,
        ContainerState::Paused => ProcessState::Paused,
        // Docker restarts a container that exits under its restart policy,
        // as a keeper restarts a host process.
        ContainerState::Restarting => ProcessState::Restarting,
        ContainerState::Exited(code) => ProcessState::exited(Some(code)),
        ContainerState::Created if planned => ProcessState::Starting,
        ContainerState::Created => ProcessState::Stopped,
        ContainerState::Unknown => ProcessState::unknown(UnknownReason::UnrecognisedState),
    }
}

/// Returns the state of a workspace whose processes are in `rows`. A
/// process whose state is unknown may or may not run, so the state comes
/// from the other processes, and is unknown when the unknown ones decide
/// it: every other process runs, or none of them does.
fn workspace_state(rows: &[StatusRow]) -> WorkspaceState {
    let unknown = rows
        .iter()
        .any(|row| matches!(row.state, ProcessState::Unknown { .. }));
    let known: Vec<ProcessState> = rows
        .iter()
        .map(|row| row.state)
        .filter(|state| !matches!(state, ProcessState::Unknown { .. }))
        .collect();
    let decided = |state| {
        if unknown {
            WorkspaceState::Unknown
        } else {
            state
        }
    };
    if known.contains(&ProcessState::Starting) {
        WorkspaceState::Starting
    } else if !known.iter().any(|state| state.may_run()) {
        decided(WorkspaceState::Stopped)
    } else if known.iter().all(|state| {
        matches!(
            state,
            ProcessState::Running | ProcessState::Ready | ProcessState::Completed
        )
    }) {
        decided(WorkspaceState::Running)
    } else {
        WorkspaceState::PartlyRunning
    }
}

#[cfg(test)]
mod tests;
