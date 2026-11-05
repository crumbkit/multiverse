//! The clock owner (BASE-5, BASE-11), which is the one module of phase that
//! sleeps.
//!
//! A lock file and a process that another program started give phase no
//! event to wait on, so the operations that wait for them poll at an
//! interval and pause here between polls.

use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// The interval at which phase polls a lock that another operation holds
/// and a process that it has signalled.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Pauses the calling thread for `interval`, between two polls of a source
/// that has no event.
pub(crate) fn pause(interval: Duration) {
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/phase/src/clock.rs is the clock owner, and it paces polls of sources that have no event (BASE-11)"
    )]
    thread::sleep(interval);
}

/// Returns the current time, against which a poll checks its deadline.
pub(crate) fn now() -> Instant {
    Instant::now()
}

/// The furthest a deadline lies ahead, which stands in for a wait too long
/// for an [`Instant`] to hold. It is a century.
const FURTHEST: Duration = Duration::from_secs(100 * 365 * 24 * 60 * 60);

/// Returns the deadline `wait` from now. A wait that an [`Instant`] cannot
/// hold, such as [`Duration::MAX`], gives a deadline a century ahead, so a
/// grace written as a huge number waits without panicking.
pub(crate) fn deadline_in(wait: Duration) -> Instant {
    let start = now();
    start
        .checked_add(wait)
        .or_else(|| start.checked_add(FURTHEST))
        .unwrap_or(start)
}

/// Returns the wall-clock time, against which phase compares a credential's
/// expiry and stamps a diagnostic line.
pub(crate) fn wall_now() -> SystemTime {
    SystemTime::now()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{FURTHEST, deadline_in, now};

    #[test]
    fn a_deadline_past_what_an_instant_holds_is_a_century_ahead() {
        let before = now();
        let deadline = deadline_in(Duration::MAX);
        assert!(deadline >= before + FURTHEST);
    }

    #[test]
    fn a_deadline_is_its_wait_from_now() {
        let before = now();
        let deadline = deadline_in(Duration::from_secs(5));
        assert!(deadline >= before + Duration::from_secs(5));
        assert!(deadline <= now() + Duration::from_secs(5));
    }
}
