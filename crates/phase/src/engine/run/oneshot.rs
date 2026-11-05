//! Oneshot host processes: whether `up` runs one again, how `up` waits for
//! one to exit, and how it records the completion.
//!
//! A oneshot runs under a keeper like any host process, with the same
//! sandbox and environment, and its output goes to its log. Its keeper
//! never restarts it, because the plan refuses a oneshot with a restart
//! policy. `up` waits until the oneshot exits, within its ready timeout
//! and the caller's readiness deadline. An exit with status 0 completes
//! it: `up` records the completion with the hash of the oneshot's part of
//! the plan, stops the keeper, and goes on to the processes and apps that
//! depend on it. Any other end fails its app with the exit code or signal
//! and the log's tail, as a process that ends before it is ready does.
//!
//! A later `up` leaves a completed oneshot alone, without running it, when
//! its completion is from the same part of the plan and either it declares
//! `once`, or every other selected process of its app is already running
//! and every app that its app depends on was already running in this `up`.
//! `restart` runs it again either way, and `down` forgets the completion.

use crate::clock::{deadline_in, now};
use crate::engine::host::is_gone;
use crate::engine::keeper::AppEnd;
use crate::engine::probe::{LogLines, Outcome};
use crate::engine::run::stop::{self, Stopped, Target, target_of};
use crate::engine::run::{END_LINE_WAIT, Op, Recorded, Step, Tail, watch};
use crate::error::PhaseError;
use crate::name::AppName;
use crate::plan::{AppPlan, ProcessKind, ProcessPlan};
use crate::record::run::{EntryState, Exit, ObjectKey, PlanHash, RunRecord};
use crate::report::{Note, StartProblem};

/// Whether `up` or `restart` may leave a completed oneshot alone.
#[derive(Clone, Copy, Debug)]
pub(super) enum Rerun {
    /// `restart` runs every selected oneshot again.
    Always,
    /// `up` leaves a oneshot alone as the module docs say. `depends_left`
    /// reports whether every app that the oneshot's app depends on was
    /// already running in this `up`.
    Up {
        /// Whether every app that the app depends on was already running.
        depends_left: bool,
    },
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
    /// `app` alone, as `rerun` and the module docs say. `others` are the
    /// selected processes of the app.
    ///
    /// # Errors
    ///
    /// Returns the errors of reading the process table.
    pub(super) fn leaves_oneshot(
        &self,
        app: &AppPlan,
        process: &ProcessPlan,
        others: &[&ProcessPlan],
        rerun: Rerun,
    ) -> Result<bool, PhaseError> {
        let ProcessKind::Oneshot { once } = process.kind else {
            return Ok(false);
        };
        let Some(record) = self.journal.record() else {
            return Ok(false);
        };
        if !completed_from_plan(record, app, process) {
            return Ok(false);
        }
        let depends_left = match rerun {
            Rerun::Always => return Ok(false),
            Rerun::Up { depends_left } => depends_left,
        };
        if once {
            return Ok(true);
        }
        if !depends_left {
            return Ok(false);
        }
        for other in others {
            if matches!(other.kind, ProcessKind::Oneshot { .. }) {
                continue;
            }
            if !matches!(self.recorded(app, other)?, Recorded::Ours) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Waits until the started oneshot `target` exits, within its ready
    /// timeout and the caller's readiness deadline. An exit with status 0
    /// completes it, and any other end fails the start with the code or
    /// signal that `end` reads from the keeper and the log's tail.
    pub(super) fn await_completion(
        &self,
        process: &ProcessPlan,
        target: &Target,
        end: AppEnd,
        lines: Box<dyn LogLines + Send>,
    ) -> Step {
        let host = self.engine.host;
        let began = now();
        let own_deadline = began + process.ready_timeout;
        let deadline = own_deadline.min(self.deadlines.ready);
        let mut tail = Tail::new(lines);
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
                        tail.finish(),
                    ))
                }
            }
            // `watch` returns ready when its deadline passes first.
            _ if own_deadline > deadline => Step::Uncertain,
            _ => Step::Failed(StartProblem::not_ready(now() - began)),
        }
    }

    /// Records that the oneshot `process` of `app`, started as `target`,
    /// completed, and stops its keeper, which waits for the lifecycle lock
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
        self.journal.completed(
            &ObjectKey::Process(app.name.clone(), process.name.clone()),
            part_hash(app, process),
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
