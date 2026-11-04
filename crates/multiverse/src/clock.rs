//! The clock owner (BASE-5, BASE-11), which is the one module that sleeps.
//!
//! An app's process, its port and its log file give multiverse no event to
//! wait on, so the operations that wait for them poll at an interval and
//! pause here between polls.

use std::thread;
use std::time::Duration;

/// The interval at which `mxv up` and `mxv down` poll a process and its
/// port.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// The interval at which `mxv logs -f` polls a log file for new lines.
pub const FOLLOW_INTERVAL: Duration = Duration::from_millis(200);

/// Pauses the calling thread for `interval`, between two polls of a source
/// that has no event.
pub(crate) fn pause(interval: Duration) {
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/clock.rs is the clock owner, and it paces polls of sources that have no event (BASE-11)"
    )]
    thread::sleep(interval);
}
