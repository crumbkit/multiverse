//! The runner, which attempts a [`Check`] at an interval and counts
//! consecutive passes and failures against the probe's thresholds.
//! [`Tally`] holds the count and decides without a clock, and [`wait`]
//! paces the attempts through [`crate::clock`].

use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use super::{Attempt, Check, Failure};
use crate::cancel::Cancel;
use crate::clock::{POLL_INTERVAL, now, pause};
use crate::plan::probe::Probe;

/// The longest time between two attempts of a readiness wait. mxv polled a
/// starting process every 20 ms, and an interval this short keeps `up` from
/// waiting a whole period of 10 s for a process that is already ready.
pub(crate) const READY_INTERVAL: Duration = Duration::from_millis(100);

/// The consecutive passes and failures of a probe's attempts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Tally {
    /// The passes in a row that make the process ready.
    success_threshold: NonZeroU32,
    /// The failures in a row that make the probe fail.
    failure_threshold: NonZeroU32,
    /// The passes in a row so far.
    passes: u32,
    /// The failures in a row so far.
    failures: u32,
}

/// What a [`Tally`] concludes after an attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The passes in a row have reached the success threshold.
    Passing,
    /// The failures in a row have reached the failure threshold.
    Failing,
    /// Neither threshold is reached.
    Undecided,
}

impl Tally {
    /// Returns a tally of no attempts under `probe`'s thresholds.
    pub(crate) const fn new(probe: &Probe) -> Self {
        Self {
            success_threshold: probe.success_threshold,
            failure_threshold: probe.failure_threshold,
            passes: 0,
            failures: 0,
        }
    }

    /// Counts one attempt that `passed` or not, and returns the verdict. A
    /// pass ends a run of failures, and a failure ends a run of passes.
    pub(crate) fn record(&mut self, passed: bool) -> Verdict {
        if passed {
            self.failures = 0;
            self.passes = self.passes.saturating_add(1);
            if self.passes >= self.success_threshold.get() {
                return Verdict::Passing;
            }
        } else {
            self.passes = 0;
            self.failures = self.failures.saturating_add(1);
            if self.failures >= self.failure_threshold.get() {
                return Verdict::Failing;
            }
        }
        Verdict::Undecided
    }
}

/// What a [`wait`] waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Goal {
    /// Readiness, during `up`. The wait attempts the check every
    /// [`READY_INTERVAL`], or every period when that is shorter, and ends
    /// when the passes in a row reach the success threshold. Failures never
    /// end it, because a process may take until its ready timeout to start
    /// listening.
    Ready,
    /// Liveness, in a keeper. The wait attempts the check once each period,
    /// and ends when the failures in a row reach the failure threshold.
    Failing,
}

/// How a [`wait`] ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The goal [`Goal::Ready`] was reached.
    Ready,
    /// The goal [`Goal::Failing`] was reached, and `last` is the reason
    /// that the last attempt failed.
    Failing {
        /// Why the last attempt failed.
        last: Failure,
    },
    /// The deadline passed before the goal was reached. `waited` is the
    /// time since the wait began, and `last` is the reason that the last
    /// attempt failed, or `None` when it passed or none was made.
    NotReady {
        /// How long the wait lasted.
        waited: Duration,
        /// Why the last attempt failed.
        last: Option<Failure>,
    },
    /// The process ended during the wait.
    Ended,
    /// The caller cancelled the wait.
    Cancelled,
}

/// Attempts `check` until `goal` is reached, `deadline` passes, `ended`
/// reports that the process has ended, or `cancel` is set. The first
/// attempt starts at once, and later attempts follow at the goal's
/// interval. No attempt outlasts the probe's timeout or the deadline.
///
/// Between attempts the wait checks `cancel` and `ended` every
/// [`POLL_INTERVAL`]. While it waits for readiness, it also lets a log
/// check read its new lines, and it checks for a match before it checks
/// whether the process ended. A log line that matches is attempted at
/// once, even when the deadline has just passed, so a process that writes
/// its ready line and then exits counts as ready.
pub(crate) fn wait(
    probe: &Probe,
    check: &mut Check<'_>,
    goal: Goal,
    deadline: Instant,
    ended: &mut dyn FnMut() -> bool,
    cancel: &Cancel,
) -> Outcome {
    let interval = match goal {
        Goal::Ready => READY_INTERVAL.min(probe.period),
        Goal::Failing => probe.period,
    };
    let began = now();
    let mut tally = Tally::new(probe);
    let mut last = None;
    loop {
        if cancel.is_cancelled() {
            return Outcome::Cancelled;
        }
        let matched = goal == Goal::Ready && check.poll();
        if !matched && ended() {
            return Outcome::Ended;
        }
        let started = now();
        let left = deadline.saturating_duration_since(started);
        if left.is_zero() && !matched {
            return Outcome::NotReady {
                waited: started.saturating_duration_since(began),
                last,
            };
        }
        let attempt = check.attempt(probe.timeout.min(left), cancel);
        if cancel.is_cancelled() {
            return Outcome::Cancelled;
        }
        let passed = attempt == Attempt::Passed;
        last = match attempt {
            Attempt::Passed => None,
            Attempt::Failed(failure) => Some(failure),
        };
        match (tally.record(passed), goal, last.clone()) {
            (Verdict::Passing, Goal::Ready, _) => return Outcome::Ready,
            (Verdict::Failing, Goal::Failing, Some(failure)) => {
                return Outcome::Failing { last: failure };
            }
            _ => {}
        }
        if matched {
            continue;
        }
        let next = (started + interval).min(deadline);
        loop {
            let remaining = next.saturating_duration_since(now());
            let matched = goal == Goal::Ready && check.poll();
            if remaining.is_zero() || cancel.is_cancelled() || ended() || matched {
                break;
            }
            pause(POLL_INTERVAL.min(remaining));
        }
    }
}
