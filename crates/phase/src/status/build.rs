//! Building a workspace's [`Status`] from its run record, the process
//! table and Docker, as the status module's docs say.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;

use crate::engine::container::ContainerState;
use crate::error::PhaseError;
use crate::logs::log_path;
use crate::name::{AppName, ProcessName, WorkspaceId};
use crate::plan::{AppPlan, Plan, ProcessPlan, RunnerKind};
use crate::record::Handle;
use crate::record::docker::{Docker, DockerKind, DockerObject};
use crate::record::process::{self, Identity, Processes, vouched};
use crate::record::run::{ContainerEntry, EntryState, HostEntry, RunRecord};
use crate::record::store::RunStore;
use crate::status::{
    ProcessState, SourceSummary, Status, StatusRow, UnknownReason, WorkspaceState,
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
            let live = [keeper, entry].into_iter().flatten().any(HostEntry::live);
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
    let state = host_state(keeper, entry, sources.processes)?;
    let mut row = StatusRow::new(app.clone(), process.clone(), RunnerKind::Host, None, state);
    if let Some(entry) = entry {
        row.restarts = entry.restarts;
        row.address = entry.listen.as_ref().map(|listen| loopback(listen.port));
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
        if started {
            ProcessState::Restarting
        } else {
            ProcessState::Starting
        }
    } else {
        ProcessState::Stopped
    })
}

/// Returns the row of the container `entry`, whose state Docker gives. A
/// Docker that does not answer makes the state unknown, never stopped.
fn container_row(
    workspace: &WorkspaceId,
    entry: &ContainerEntry,
    sources: Sources<'_>,
) -> StatusRow {
    let state = if entry.state == EntryState::Stopped {
        Ok(None)
    } else {
        sources.docker.container_state(&DockerObject {
            workspace,
            kind: DockerKind::Container,
            name: &entry.name,
            id: entry.id.as_ref().map(crate::record::ContainerId::as_str),
        })
    };
    // A container that the record plans and Docker has not started yet is
    // starting, as a host process whose keeper has not started it is.
    let planned = entry.state == EntryState::Planned;
    let state = match state {
        Ok(Some(ContainerState::Running)) => ProcessState::Running,
        Ok(Some(ContainerState::Paused)) => ProcessState::Paused,
        // Docker restarts a container that exits under its restart policy,
        // as a keeper restarts a host process.
        Ok(Some(ContainerState::Restarting)) => ProcessState::Restarting,
        Ok(Some(ContainerState::Exited(code))) => ProcessState::exited(Some(code)),
        Ok(Some(ContainerState::Created) | None) if planned => ProcessState::Starting,
        Ok(Some(ContainerState::Created) | None) => ProcessState::Stopped,
        Ok(Some(ContainerState::Unknown)) => {
            ProcessState::unknown(UnknownReason::UnrecognisedState)
        }
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
