//! Stopping: `down`, and the stops that `up` and `restart` make.
//!
//! `down` reads what to stop from the run record alone, so it needs no
//! plan. It stops the selected apps in the reverse of their dependency
//! order, and the processes of each app in the reverse of their start
//! order. It treats a keeper and its process as one pair, which may still
//! run while either entry is not `stopped`: a keeper that waits to restart
//! its app has recorded the app as stopped while the keeper itself runs.
//! It records each entry as `stopping` before it acts and `stopped` once
//! the host runner has checked that it is gone. A process
//! that phase cannot prove is its own, or that survives `KILL`, keeps its
//! entry, and the report notes it. After a process's group is gone, `down`
//! checks its port, and notes a process that still holds it without
//! signalling that process. The record is removed once everything in it is
//! stopped.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use crate::clock::now;
use crate::engine::containers::{ContainerStop, ContainerStopped};
use crate::engine::host::{HostStop, HostStopped};
use crate::engine::run::order::stopping_order;
use crate::engine::run::{Engine, LookupFailed, PortHolder, port_holder};
use crate::error::PhaseError;
use crate::name::{AppName, ProcessName, WorkspaceId};
use crate::plan::ProcessPlan;
use crate::plan::restart::Stop;
use crate::record::process::Identity;
use crate::record::run::{
    ContainerEntry, EntryState, HostEntry, HostProcess, Listen, ObjectKey, RunRecord,
};
use crate::record::store::{Journal, RunStore};
use crate::report::{AppOutcome, AppReport, Note, Report};
use crate::select::Selection;

/// A host process to stop: its keeper and itself as the record names them,
/// and how it stops.
#[derive(Clone, Debug)]
pub(super) struct Target {
    /// The app.
    pub(super) app: AppName,
    /// The process.
    pub(super) process: ProcessName,
    /// The keeper, or `None` when it never started.
    pub(super) keeper: Option<HostProcess>,
    /// The process, or `None` when it never started.
    pub(super) host: Option<HostProcess>,
    /// How the process stops.
    pub(super) stop: Stop,
    /// The port it listens on, or `None`.
    pub(super) listen: Option<Listen>,
}

/// How a stop of one [`Target`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stopped {
    /// Nothing of the keeper or the process is left.
    Stopped,
    /// phase left something that it could not stop or could not prove is
    /// its own, and the record keeps it.
    Left,
}

/// Stops `target` through the engine's host runner, giving up its first
/// wait at `deadline`, records each step in `journal`, and adds what it
/// found to `notes`.
///
/// A record that cannot be written does not keep phase from stopping the
/// process. The record still names the process as running, which is safe,
/// and `stop_target` returns the first write error after the stop.
///
/// # Errors
///
/// Returns the first error of writing the record, and
/// [`PhaseError::ProcessTable`] when the process table cannot be read or a
/// signal cannot be sent.
pub(super) fn stop_target(
    engine: &Engine<'_>,
    journal: &mut Journal<'_>,
    target: &Target,
    deadline: Instant,
    notes: &mut Vec<Note>,
) -> Result<Stopped, PhaseError> {
    let keys = [
        ObjectKey::Keeper(target.app.clone(), target.process.clone()),
        ObjectKey::Process(target.app.clone(), target.process.clone()),
    ];
    let named: Vec<&ObjectKey> = keys
        .iter()
        .filter(|key| {
            journal
                .record()
                .is_some_and(|record| record.state(key).is_some())
        })
        .collect();
    let mut first_error = None;
    for key in &named {
        if let Err(error) = journal.stopping(key) {
            first_error.get_or_insert(error);
        }
    }
    let stopped = stop_host(engine, journal, target, deadline, notes, &named);
    match (first_error, stopped) {
        (Some(error), _) | (None, Err(error)) => Err(error),
        (None, Ok(stopped)) => Ok(stopped),
    }
}

/// Stops `target` through the engine's host runner, and records the
/// entries `named` as stopped once it is gone.
fn stop_host(
    engine: &Engine<'_>,
    journal: &mut Journal<'_>,
    target: &Target,
    deadline: Instant,
    notes: &mut Vec<Note>,
    named: &[&ObjectKey],
) -> Result<Stopped, PhaseError> {
    let stop = HostStop {
        keeper: target.keeper.as_ref(),
        app: target.host.as_ref(),
        signal: target.stop.signal,
        grace: target.stop.grace,
        kill_wait: engine.settings.kill_wait,
    };
    match engine.host.stop(&stop, deadline)? {
        HostStopped::Stopped | HostStopped::AlreadyGone => {
            for key in named {
                journal.stopped(key)?;
            }
            if let Some(listen) = &target.listen {
                let group = target.host.as_ref().map_or(0, |host| host.group);
                match port_holder(engine.host, group, listen.port, LookupFailed::Report) {
                    PortHolder::Free => {}
                    PortHolder::Group(pid) | PortHolder::Other(pid) => {
                        notes.push(Note::detached_listener(
                            target.process.clone(),
                            listen.name.clone(),
                            pid.into_iter().collect(),
                        ));
                    }
                    PortHolder::Unknown(error) => notes.push(Note::listener_unknown(
                        target.process.clone(),
                        listen.name.clone(),
                        error.to_string(),
                    )),
                }
            }
            Ok(Stopped::Stopped)
        }
        HostStopped::NotOurs => {
            notes.push(Note::not_ours(target.process.clone()));
            Ok(Stopped::Left)
        }
        HostStopped::GroupUnverified { group } => {
            notes.push(Note::group_unverified(target.process.clone(), group));
            Ok(Stopped::Left)
        }
        HostStopped::Unstoppable { group } => {
            notes.push(Note::unstoppable(target.process.clone(), group));
            Ok(Stopped::Left)
        }
    }
}

/// Returns the target of the record's `keeper` and `process` entries of
/// one host process, naming each one that is not stopped, or `None` when
/// both are missing.
pub(super) fn target_of(keeper: Option<&HostEntry>, process: Option<&HostEntry>) -> Option<Target> {
    let named = keeper.or(process)?;
    let live = |entry: Option<&HostEntry>| {
        entry
            .filter(|entry| entry.live())
            .and_then(|entry| entry.host.clone())
    };
    Some(Target {
        app: named.app.clone(),
        process: named.process.clone(),
        keeper: live(keeper),
        host: live(process),
        stop: process.and_then(|entry| entry.stop).unwrap_or_default(),
        listen: process.and_then(|entry| entry.listen.clone()),
    })
}

/// Returns the targets of the `processes` of `app` whose keeper or process
/// `record` names as not stopped and is still one that phase started, in
/// the order of `processes`.
///
/// # Errors
///
/// Returns [`PhaseError::ProcessTable`] when the process table cannot be
/// read.
pub(super) fn running_targets(
    engine: &Engine<'_>,
    record: Option<&RunRecord>,
    app: &AppName,
    processes: &[&ProcessPlan],
) -> Result<Vec<Target>, PhaseError> {
    let Some(record) = record else {
        return Ok(Vec::new());
    };
    let mut targets = Vec::new();
    for process in processes {
        let (keeper, entry) = record.host_pair(app, &process.name);
        let Some(target) = target_of(keeper, entry) else {
            continue;
        };
        let mut ours = false;
        // A group whose leader has gone is a target too, so the stop
        // decides whether anything proves it ours and notes it when not.
        for host in [&target.keeper, &target.host].into_iter().flatten() {
            ours |= matches!(
                engine.host.identity(host)?,
                Identity::Ours | Identity::LeaderGone
            );
        }
        if ours {
            targets.push(target);
        }
    }
    Ok(targets)
}

/// Stops the selected apps of `workspace`, as the module docs say.
///
/// # Errors
///
/// Returns the errors of taking the lock, of reading and writing the
/// record, and of the host runner's stop.
pub(super) fn down(
    engine: &Engine<'_>,
    workspace: &WorkspaceId,
    select: &Selection,
    deadline: Instant,
) -> Result<Report, PhaseError> {
    let store = RunStore::new(engine.settings.state_dir);
    let mut journal = Journal::open(&store, workspace, engine.settings.lock_wait)?;
    let record = journal.record().cloned().unwrap_or_else(|| {
        RunRecord::new(
            workspace.clone(),
            crate::record::run::PlanHash::new([0; 32]),
        )
    });
    let order = stopping_order(&record);
    // `None` stands for every process of the app.
    let wanted: Option<BTreeMap<&AppName, Option<BTreeSet<&ProcessName>>>> = match select {
        Selection::All => None,
        Selection::Only(targets) => {
            let mut wanted: BTreeMap<&AppName, Option<BTreeSet<&ProcessName>>> = BTreeMap::new();
            for target in targets {
                let slot = wanted
                    .entry(&target.app)
                    .or_insert_with(|| Some(BTreeSet::new()));
                match (&target.process, slot.as_mut()) {
                    (None, _) => *slot = None,
                    (Some(process), Some(set)) => {
                        set.insert(process);
                    }
                    (Some(_), None) => {}
                }
            }
            Some(wanted)
        }
    };
    let mut reports = Vec::new();
    if let Some(wanted) = &wanted {
        for app in wanted.keys() {
            if !order.contains(app) {
                reports.push(AppReport::new((*app).clone(), AppOutcome::NotRunning));
            }
        }
    }
    for app in &order {
        let only = match &wanted {
            None => None,
            Some(wanted) => match wanted.get(app) {
                None => continue,
                Some(only) => only.as_ref(),
            },
        };
        let chosen = |process: &ProcessName| only.is_none_or(|only| only.contains(process));
        let report = down_app(engine, &mut journal, &record, app, &chosen, deadline)?;
        reports.push(report);
    }
    let keeps_an_exit = wanted.as_ref().is_some_and(|wanted| {
        journal
            .record()
            .is_some_and(|record| record.holds_an_exit_outside(wanted))
    });
    if !keeps_an_exit {
        journal.finish()?;
    }
    Ok(Report::new(reports))
}

/// Stops the processes of `app` that `chosen` accepts, and reports what
/// happened to the app.
fn down_app(
    engine: &Engine<'_>,
    journal: &mut Journal<'_>,
    record: &RunRecord,
    app: &AppName,
    chosen: &dyn Fn(&ProcessName) -> bool,
    deadline: Instant,
) -> Result<AppReport, PhaseError> {
    let mut notes = Vec::new();
    let mut any = false;
    let mut left = false;
    let pairs = record.host_pairs();
    let targets = pairs.iter().rev().filter_map(|(keeper, process)| {
        let target = target_of(*keeper, *process)?;
        let live = keeper.is_some_and(HostEntry::live) || process.is_some_and(HostEntry::live);
        (&target.app == app && chosen(&target.process) && live).then_some(target)
    });
    for target in targets {
        if target.keeper.is_some() || target.host.is_some() {
            any = true;
        }
        let stopped = stop_target(engine, journal, &target, deadline, &mut notes)?;
        left |= stopped == Stopped::Left;
    }
    let containers: Vec<ContainerEntry> = record
        .containers
        .iter()
        .filter(|entry| {
            &entry.app == app && chosen(&entry.process) && entry.state != EntryState::Stopped
        })
        .cloned()
        .collect();
    if !containers.is_empty() {
        any = true;
        match stop_containers(engine, journal, app, &containers, deadline) {
            Ok(all) => left |= !all,
            Err(error) => {
                return Ok(
                    AppReport::new(app.clone(), AppOutcome::errored(&error)).with_notes(notes)
                );
            }
        }
    }
    let outcome = match (any, left) {
        (_, true) if now() >= deadline => AppOutcome::StopUncertain,
        (_, true) => AppOutcome::Left,
        (true, false) => AppOutcome::Stopped,
        (false, false) => AppOutcome::NotRunning,
    };
    Ok(AppReport::new(app.clone(), outcome).with_notes(notes))
}

/// Stops the recorded `containers` of `app` through the engine's container
/// runner, in reverse of their order in the record, which is dependency
/// order, records each one that is gone, and reports whether all are.
///
/// # Errors
///
/// Returns the container runner's first failure, after it records each
/// container that is gone, and the errors of writing the record.
pub(super) fn stop_containers(
    engine: &Engine<'_>,
    journal: &mut Journal<'_>,
    app: &AppName,
    containers: &[ContainerEntry],
    deadline: Instant,
) -> Result<bool, PhaseError> {
    let workspace = containers
        .first()
        .and_then(|_| journal.record().map(|record| record.workspace.clone()));
    let Some(workspace) = workspace else {
        return Ok(true);
    };
    for entry in containers {
        journal.stopping(&ObjectKey::Container(app.clone(), entry.process.clone()))?;
    }
    let stopped = engine.containers.stop(&ContainerStop {
        workspace: &workspace,
        app,
        containers,
        deadline,
    });
    let mut all = true;
    let mut failure = None;
    for (process, how) in stopped {
        match how {
            Ok(ContainerStopped::Stopped | ContainerStopped::AlreadyGone) => {
                journal.stopped(&ObjectKey::Container(app.clone(), process))?;
            }
            Ok(ContainerStopped::Unstoppable) => all = false,
            Err(error) => {
                all = false;
                failure.get_or_insert(error);
            }
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(all),
    }
}
