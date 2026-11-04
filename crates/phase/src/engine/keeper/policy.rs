//! What a keeper does after its app ends: the restart policy, the backoff
//! and whether a restart needs a fresh environment. These are pure
//! decisions, which the supervisor carries out.

use std::time::{Duration, SystemTime};

use crate::engine::keeper::KeeperExit;
use crate::plan::restart::{Backoff, Resolve, Restart, RestartLimits};

#[cfg(test)]
mod tests;

/// What the keeper does after its app ended by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AfterExit {
    /// The keeper waits `delay`, then restarts the app, unless `resolve`
    /// says the restart needs a fresh environment.
    Restart {
        /// The policy's backoff, from which the supervisor takes the wait
        /// before the restart.
        backoff: Backoff,
        /// Whether the restart resolves the environment again.
        resolve: Resolve,
    },
    /// The keeper leaves the app ended and exits so.
    Leave(KeeperExit),
}

/// Returns what the keeper does after its app ended, `failed` or not, when
/// the app has been restarted `restarts` times already under `restart`. A
/// failed attempt to resolve the environment counts as a failed run.
pub(crate) fn after_exit(restart: &Restart, failed: bool, restarts: u32) -> AfterExit {
    let leave = AfterExit::Leave(if failed {
        KeeperExit::Failed
    } else {
        KeeperExit::Completed
    });
    let limits: &RestartLimits = match restart {
        Restart::No => return leave,
        Restart::OnFailure(_) if !failed => return leave,
        Restart::OnFailure(limits) | Restart::Always(limits) => limits,
    };
    if limits.max.is_some_and(|max| restarts >= max.get()) {
        return leave;
    }
    AfterExit::Restart {
        backoff: limits.backoff,
        resolve: limits.resolve,
    }
}

/// The shortest run after which a process counts as healthy, unless the
/// backoff's longest wait is longer.
pub(crate) const HEALTHY_RUN: Duration = Duration::from_secs(60);

/// Returns the wait before a restart that follows `streak` restarts in a
/// row: the backoff's first wait, doubled once for each of them, and never
/// longer than its longest wait.
pub(crate) fn backoff_delay(backoff: Backoff, streak: u32) -> Duration {
    let factor = 2_u32.checked_pow(streak).unwrap_or(u32::MAX);
    backoff
        .first()
        .checked_mul(factor)
        .map_or(backoff.most(), |delay| delay.min(backoff.most()))
}

/// Returns the count of restarts in a row after a run that lasted `ran`
/// and followed `streak` of them. A run longer than the backoff's longest
/// wait, or than [`HEALTHY_RUN`] when that is longer, was healthy, and the
/// count starts again from 0, so the next wait is the first one.
pub(crate) fn streak_after_run(streak: u32, ran: Duration, backoff: Backoff) -> u32 {
    if ran > backoff.most().max(HEALTHY_RUN) {
        0
    } else {
        streak
    }
}

/// Reports whether a restart under `resolve` needs the environment
/// resolved again at `now`, when a value of it expires at `expires_at`.
pub(crate) fn needs_environment(
    resolve: Resolve,
    now: SystemTime,
    expires_at: Option<SystemTime>,
) -> bool {
    match resolve {
        Resolve::Always => true,
        Resolve::Never => false,
        Resolve::WhenExpired => expires_at.is_some_and(|expires| expires <= now),
    }
}
