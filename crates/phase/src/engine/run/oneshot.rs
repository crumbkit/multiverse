//! Oneshot host processes: whether `up` runs one again, how `up` waits for
//! one to exit, and how it records the completion.
//!
//! A oneshot runs under a keeper like any host process, with the same
//! sandbox and environment, and its output goes to its log. Its keeper
//! never restarts it, because the plan refuses a oneshot with a restart
//! policy. When phase launches a oneshot, it records the hash of the
//! oneshot's part of the plan and, for a `once = false` oneshot, the run
//! of each process and container that it depends on, which
//! [`Op::dependency_runs`] describes.
//!
//! `up` waits until the oneshot exits, within its ready timeout and the
//! caller's readiness deadline. An exit with status 0 completes it: `up`
//! records the end, which records the completion, stops the keeper, and
//! goes on to the processes and apps that depend on it. When `up` stops
//! waiting first, the keeper records the end itself once it takes the
//! lifecycle lock, and an exit with status 0 completes the oneshot then.
//! Any other end fails its app with the exit code or signal, as a process
//! that ends before it is ready does. The failure holds nothing that the
//! process wrote, because that output may hold a secret value. The log
//! holds it.
//!
//! A later `up` leaves a completed oneshot alone, without running it, when
//! its completion is from the same part of the plan and either it declares
//! `once`, or every process and container that it depends on runs as the
//! same run that phase recorded when it launched the oneshot. `restart`
//! runs it again either way, and `down` forgets the completion.

use crate::clock::{deadline_in, now};
use crate::engine::host::is_gone;
use crate::engine::keeper::AppEnd;
use crate::engine::probe::Outcome;
use crate::engine::run::order::sorted;
use crate::engine::run::stop::{self, Stopped, Target, target_of};
use crate::engine::run::{END_LINE_WAIT, Op, Step, watch};
use crate::error::PhaseError;
use crate::name::AppName;
use crate::plan::{AppPlan, ProcessKind, ProcessPlan};
use crate::record::process::Identity;
use crate::record::run::{
    DependencyRun, Ended, EntryState, Exit, ObjectKey, OneshotLaunch, PlanHash, RunRecord,
};
use crate::report::{Note, StartProblem};
use std::collections::BTreeSet;

/// Whether `up` or `restart` may leave a completed oneshot alone.
#[derive(Clone, Copy, Debug)]
pub(super) enum Rerun {
    /// `restart` runs every selected oneshot again.
    Always,
    /// `up` leaves a oneshot alone as the module docs say.
    Up,
}

/// Returns the hash of the part of the plan that `process` of `app` runs
/// from, which a completion records.
pub(super) fn part_hash(app: &AppPlan, process: &ProcessPlan) -> PlanHash {
    PlanHash::new(app.process_digest(process))
}

/// Reports whether `record` holds a completion of `process` of `app` from
/// its current part of the plan, and names the process as stopped.
fn completed_from_plan(record: &RunRecord, app: &AppPlan, process: &ProcessPlan) -> bool {
    let (_, entry) = record.host_pair(&app.name, &process.name);
    entry.is_some_and(|entry| {
        entry.state == EntryState::Stopped
            && entry.completed.as_ref() == Some(&part_hash(app, process))
    })
}

/// Returns the targets of the selected `processes` of `app` that are
/// oneshots whose completion `record` holds, in the order of `processes`.
/// Neither the keeper nor the process of such a target runs.
pub(super) fn completed_targets(
    record: Option<&RunRecord>,
    app: &AppName,
    processes: &[&ProcessPlan],
) -> Vec<Target> {
    let Some(record) = record else {
        return Vec::new();
    };
    processes
        .iter()
        .filter(|process| matches!(process.kind, ProcessKind::Oneshot { .. }))
        .filter_map(|process| {
            let (keeper, entry) = record.host_pair(app, &process.name);
            entry
                .filter(|entry| entry.completed.is_some() && !entry.live())
                .and_then(|entry| target_of(keeper, Some(entry)))
        })
        .collect()
}

impl Op<'_, '_> {
    /// Reports whether this operation leaves the oneshot `process` of
    /// `app` alone, as `rerun` and the module docs say.
    pub(super) fn leaves_oneshot(
        &self,
        app: &AppPlan,
        process: &ProcessPlan,
        rerun: Rerun,
    ) -> bool {
        let ProcessKind::Oneshot { once } = process.kind else {
            return false;
        };
        let Some(record) = self.journal.record() else {
            return false;
        };
        if matches!(rerun, Rerun::Always) || !completed_from_plan(record, app, process) {
            return false;
        }
        if once {
            return true;
        }
        let (_, entry) = record.host_pair(&app.name, &process.name);
        let launched = entry
            .and_then(|entry| entry.oneshot.as_ref())
            .and_then(|launch| launch.depends.as_ref());
        launched.is_some() && launched == self.dependency_runs(app, process).as_ref()
    }

    /// Returns what phase records when it launches `process` of `app`, or
    /// `None` for a process that is not a oneshot.
    pub(super) fn oneshot_launch(
        &self,
        app: &AppPlan,
        process: &ProcessPlan,
    ) -> Option<OneshotLaunch> {
        let ProcessKind::Oneshot { once } = process.kind else {
            return None;
        };
        Some(OneshotLaunch {
            plan_hash: part_hash(app, process),
            depends: if once {
                None
            } else {
                self.dependency_runs(app, process)
            },
        })
    }

    /// Returns the run of each process and container that the oneshot
    /// `process` of `app` depends on and that runs now, sorted by app and
    /// process, or `None` when phase cannot tell.
    ///
    /// The oneshot depends on every process and container of each app
    /// that its app depends on, directly or through another app, and on
    /// each long-running process of its own app that phase starts before
    /// it, in the dependency order of the app's processes. A host
    /// process's run is its keeper and the process that the keeper runs,
    /// which phase reads from the run record and checks still run. When
    /// the keeper runs and its process does not run as recorded, such as
    /// after a restart by the keeper that the record does not hold yet,
    /// phase cannot tell and returns `None`. A container's run is its id and when
    /// Docker last started it, which Docker reports now, so a start that
    /// phase did not make counts too.
    pub(super) fn dependency_runs(
        &self,
        app: &AppPlan,
        process: &ProcessPlan,
    ) -> Option<Vec<DependencyRun>> {
        let Some(record) = self.journal.record() else {
            return Some(Vec::new());
        };
        let mut apps = BTreeSet::new();
        let mut next: Vec<AppName> = app.depends.clone();
        while let Some(name) = next.pop() {
            if apps.insert(name.clone())
                && let Some(entry) = record.apps.iter().find(|entry| entry.name == name)
            {
                next.extend(entry.depends.iter().cloned());
            }
        }
        let earlier: BTreeSet<_> = sorted(
            &app.processes,
            |process| &process.name,
            |process| &process.depends,
        )
        .into_iter()
        .take_while(|other| other.name != process.name)
        .filter(|other| other.kind == ProcessKind::LongRunning)
        .map(|other| &other.name)
        .collect();
        let mut runs = Vec::new();
        for keeper in &record.keepers {
            let depended = apps.contains(&keeper.app)
                || (keeper.app == app.name && earlier.contains(&keeper.process));
            let Some(keeper_host) = keeper.host.as_ref().filter(|_| depended && keeper.live())
            else {
                continue;
            };
            if self.engine.host.identity(keeper_host).ok()? != Identity::Ours {
                continue;
            }
            // A keeper that runs keeps its app, so the app's own run must
            // be recorded and still run. A restart by the keeper changes
            // it, and one that the keeper has not recorded yet leaves the
            // recorded process gone.
            let (_, entry) = record.host_pair(&keeper.app, &keeper.process);
            let host = entry
                .filter(|entry| entry.live())
                .and_then(|entry| entry.host.as_ref())?;
            if self.engine.host.identity(host).ok()? != Identity::Ours {
                return None;
            }
            runs.push(DependencyRun::Host {
                app: keeper.app.clone(),
                process: keeper.process.clone(),
                keeper: keeper_host.clone(),
                host: host.clone(),
            });
        }
        for name in &apps {
            let containers: Vec<_> = record
                .containers
                .iter()
                .filter(|entry| {
                    &entry.app == name && entry.id.is_some() && entry.state != EntryState::Stopped
                })
                .cloned()
                .collect();
            if containers.is_empty() {
                continue;
            }
            let started = self
                .engine
                .containers
                .started_at(&self.workspace, name, &containers)
                .ok()?;
            for (process, started_at) in started {
                let Some(id) = containers
                    .iter()
                    .find(|entry| entry.process == process)
                    .and_then(|entry| entry.id.clone())
                else {
                    continue;
                };
                runs.push(DependencyRun::Container {
                    app: name.clone(),
                    process,
                    id,
                    started_at,
                });
            }
        }
        runs.sort_by(|a, b| a.key().cmp(&b.key()));
        Some(runs)
    }

    /// Waits until the started oneshot `target` exits, within its ready
    /// timeout and the caller's readiness deadline. An exit with status 0
    /// completes it, and any other end fails the start with the code or
    /// signal that `end` reads from the keeper. It reads nothing from the
    /// log, because the failure holds nothing that the process wrote.
    pub(super) fn await_completion(
        &self,
        process: &ProcessPlan,
        target: &Target,
        end: AppEnd,
    ) -> Step {
        let host = self.engine.host;
        let began = now();
        let own_deadline = began + process.ready_timeout;
        let deadline = own_deadline.min(self.deadlines.ready);
        let mut ended = || {
            target
                .host
                .as_ref()
                .is_some_and(|app| host.identity(app).is_ok_and(is_gone))
        };
        match watch(deadline, &mut ended, self.cancel) {
            Outcome::Cancelled => Step::Cancelled,
            Outcome::Ended => {
                let exit = end.wait(now() + END_LINE_WAIT, self.cancel);
                if exit == Some(Exit::Code(0)) {
                    Step::Completed
                } else {
                    Step::Failed(StartProblem::exited(
                        exit.and_then(Exit::code),
                        exit.and_then(Exit::signal),
                    ))
                }
            }
            // `watch` returns ready when its deadline passes first.
            _ if own_deadline > deadline => Step::Uncertain,
            _ => Step::Failed(StartProblem::not_ready(now() - began)),
        }
    }

    /// Records that the oneshot `process` of `app`, started as `target`,
    /// exited with status 0, which records its completion, and stops its
    /// keeper, which waits for the lifecycle lock
    /// to record the end itself. It returns [`Step::Completed`], or
    /// [`Step::Left`] when the keeper could not be stopped, and then the
    /// operation still counts the keeper among what it started.
    ///
    /// # Errors
    ///
    /// Returns the errors of writing the record and of the host runner's
    /// stop.
    pub(super) fn complete(
        &mut self,
        app: &AppPlan,
        process: &ProcessPlan,
        target: &Target,
        notes: &mut Vec<Note>,
    ) -> Result<Step, PhaseError> {
        self.journal.ending(
            &ObjectKey::Process(app.name.clone(), process.name.clone()),
            Ended::by_itself(Some(Exit::Code(0))),
        )?;
        let kill_wait = self.engine.settings.kill_wait;
        let deadline = deadline_in(
            target
                .stop
                .grace
                .saturating_add(kill_wait.saturating_mul(2)),
        );
        let stopped = stop::stop_target(self.engine, &mut self.journal, target, deadline, notes)?;
        if stopped != Stopped::Stopped {
            return Ok(Step::Left);
        }
        // The keeper is gone, so a later failure of this operation has
        // nothing of the oneshot left to stop.
        if let Some(at) = self.started.iter().rposition(|begun| {
            matches!(begun, super::Begun::Host(started)
                if started.app == target.app && started.process == target.process)
        }) {
            self.started.remove(at);
        }
        Ok(Step::Completed)
    }
}
