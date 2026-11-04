//! The runner, which attempts a [`Check`] once each period and counts
//! consecutive passes and failures against the probe's thresholds.
//! [`Tally`] holds the count and decides without a clock, and [`wait`]
//! paces the attempts through [`crate::clock`].

use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use super::{Attempt, Check, Failure};
use crate::cancel::Cancel;
use crate::clock::{POLL_INTERVAL, now, pause};
use crate::plan::probe::Probe;

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
    /// Readiness, during `up`. The wait ends when the passes in a row reach
    /// the success threshold. Failures never end it, because a process may
    /// take until its ready timeout to start listening.
    Ready,
    /// Liveness, in a keeper. The wait ends when the failures in a row
    /// reach the failure threshold.
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

/// Attempts `check` once every period of `probe` until `goal` is reached,
/// `deadline` passes, `ended` reports that the process has ended, or
/// `cancel` is set. The first attempt starts at once. No attempt outlasts
/// the probe's timeout or the deadline. Between attempts the wait checks
/// `cancel` and `ended` every [`POLL_INTERVAL`]. While it waits for
/// readiness, it also lets a log check read its new lines then, so the
/// next attempt starts as soon as a matching line arrives instead of at
/// the next period.
pub(crate) fn wait(
    probe: &Probe,
    check: &mut Check<'_>,
    goal: Goal,
    deadline: Instant,
    ended: &mut dyn FnMut() -> bool,
    cancel: &Cancel,
) -> Outcome {
    let began = now();
    let mut tally = Tally::new(probe);
    let mut last = None;
    loop {
        if cancel.is_cancelled() {
            return Outcome::Cancelled;
        }
        if ended() {
            return Outcome::Ended;
        }
        let started = now();
        let left = deadline.saturating_duration_since(started);
        if left.is_zero() {
            return Outcome::NotReady {
                waited: started.saturating_duration_since(began),
                last,
            };
        }
        let attempt = check.attempt(probe.timeout.min(left));
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
        let next = (started + probe.period).min(deadline);
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
