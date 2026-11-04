use std::num::NonZeroU32;
use std::time::{Duration, SystemTime};

use super::{AfterExit, after_exit, backoff_delay, needs_environment};
use crate::engine::keeper::KeeperExit;
use crate::plan::restart::{Backoff, Resolve, Restart, RestartLimits};

fn limits(max: Option<u32>, resolve: Resolve) -> RestartLimits {
    RestartLimits {
        max: max.and_then(NonZeroU32::new),
        backoff: Backoff::new(Duration::from_millis(100), Duration::from_millis(1000)).unwrap(),
        resolve,
    }
}

#[test]
fn the_backoff_doubles_from_the_first_wait_up_to_the_longest() {
    let backoff = Backoff::new(Duration::from_millis(100), Duration::from_millis(1000)).unwrap();

    let delays: Vec<u128> = (0..6)
        .map(|restarts| backoff_delay(backoff, restarts).as_millis())
        .collect();

    assert_eq!(delays, vec![100, 200, 400, 800, 1000, 1000]);
    assert_eq!(
        backoff_delay(backoff, u32::MAX),
        Duration::from_millis(1000)
    );
}

#[test]
fn the_policy_no_leaves_an_ended_process_ended_with_its_outcome() {
    assert_eq!(
        after_exit(&Restart::No, false, 0),
        AfterExit::Leave(KeeperExit::Completed)
    );
    assert_eq!(
        after_exit(&Restart::No, true, 0),
        AfterExit::Leave(KeeperExit::Failed)
    );
}

#[test]
fn on_failure_restarts_a_failed_process_and_leaves_one_that_exited_cleanly() {
    let restart = Restart::on_failure(limits(None, Resolve::Never));

    assert_eq!(
        after_exit(&restart, true, 2),
        AfterExit::Restart {
            delay: Duration::from_millis(400),
            resolve: Resolve::Never,
        }
    );
    assert_eq!(
        after_exit(&restart, false, 0),
        AfterExit::Leave(KeeperExit::Completed)
    );
}

#[test]
fn always_restarts_a_process_that_exited_cleanly() {
    let restart = Restart::always(limits(None, Resolve::WhenExpired));

    assert!(matches!(
        after_exit(&restart, false, 0),
        AfterExit::Restart { .. }
    ));
}

#[test]
fn a_policy_stops_restarting_once_it_reaches_its_most_restarts() {
    let restart = Restart::always(limits(Some(2), Resolve::Never));

    assert!(matches!(
        after_exit(&restart, true, 1),
        AfterExit::Restart { .. }
    ));
    assert_eq!(
        after_exit(&restart, true, 2),
        AfterExit::Leave(KeeperExit::Failed)
    );
    assert_eq!(
        after_exit(&restart, false, 2),
        AfterExit::Leave(KeeperExit::Completed)
    );
}

#[test]
fn a_restart_that_resolves_always_needs_a_fresh_environment() {
    assert!(needs_environment(Resolve::Always, SystemTime::now(), None));
}

#[test]
fn a_restart_that_never_resolves_keeps_an_expired_environment() {
    let now = SystemTime::now();

    assert!(!needs_environment(Resolve::Never, now, Some(now)));
}

#[test]
fn a_restart_that_resolves_when_expired_needs_a_fresh_environment_only_after_expiry() {
    let now = SystemTime::now();
    let later = now + Duration::from_secs(60);

    assert!(!needs_environment(Resolve::WhenExpired, now, None));
    assert!(!needs_environment(Resolve::WhenExpired, now, Some(later)));
    assert!(needs_environment(Resolve::WhenExpired, later, Some(later)));
}
