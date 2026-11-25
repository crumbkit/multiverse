//! The clock owner (BASE-5, BASE-11), which is the one module that reads
//! the time and the one module that sleeps.
//!
//! An app's process, its port and its log file give multiverse no event to
//! wait on, so the operations that wait for them poll at an interval and
//! pause here between polls.

use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// The interval at which `mxv up` and `mxv down` poll a process and its
/// port.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Pauses the calling thread for `interval`, between two polls of a source
/// that has no event.
pub(crate) fn pause(interval: Duration) {
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/clock.rs is the clock owner, and it paces polls of sources that have no event (BASE-11)"
    )]
    thread::sleep(interval);
}

/// Returns the current time, against which a wait checks its deadline and
/// an operation measures how long it took.
#[expect(
    clippy::disallowed_methods,
    reason = "crates/multiverse/src/clock.rs is the clock owner, and it reads the time (BASE-5)"
)]
pub(crate) fn now() -> Instant {
    Instant::now()
}

/// Returns the wall-clock time, which a record stamps as the time a file
/// was written or a workspace was archived.
#[expect(
    clippy::disallowed_methods,
    reason = "crates/multiverse/src/clock.rs is the clock owner, and it reads the time (BASE-5)"
)]
pub(crate) fn wall_now() -> SystemTime {
    SystemTime::now()
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::{now, wall_now};

    #[test]
    fn the_clock_never_runs_backwards() {
        let first = now();
        let second = now();
        assert!(second >= first);
    }

    #[test]
    fn the_wall_clock_reads_a_time_after_2020() {
        let since_epoch = wall_now().duration_since(UNIX_EPOCH).unwrap();
        assert!(since_epoch > Duration::from_secs(1_577_836_800));
    }
}
