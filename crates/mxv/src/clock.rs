//! The clock owner of mxv (BASE-5). Every production site of mxv that reads
//! the time calls this module, so the program has one place that reads the
//! clock.

use std::time::{Instant, SystemTime};

/// Returns the current monotonic time, against which mxv starts a deadline
/// or measures how long a step took.
pub(crate) fn now() -> Instant {
    Instant::now()
}

/// Returns the wall-clock time, against which mxv measures how long ago a
/// checkout was modified, a workspace was used, a mirror was fetched or an
/// archive was made.
pub(crate) fn wall_now() -> SystemTime {
    SystemTime::now()
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use super::{now, wall_now};

    #[test]
    fn the_monotonic_time_never_goes_back() {
        let first = now();
        let second = now();
        assert!(second >= first);
    }

    #[test]
    fn the_wall_clock_time_is_after_the_unix_epoch() {
        assert!(wall_now() > UNIX_EPOCH);
    }
}
