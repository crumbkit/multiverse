//! The clock owner (BASE-5, BASE-11), which is the one module of phase that
//! sleeps.
//!
//! A lock file and a process that another program started give phase no
//! event to wait on, so the operations that wait for them poll at an
//! interval and pause here between polls.

use std::thread;
use std::time::Duration;

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
