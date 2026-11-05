use std::num::NonZeroU32;
use std::time::{Duration, SystemTime};

use super::{
    AfterExit, HEALTHY_RUN, RunEnd, after_exit, backoff_delay, needs_environment, streak_after_run,
};
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
        AfterExit::Leave(RunEnd::Completed)
    );
    assert_eq!(
        after_exit(&Restart::No, true, 0),
        AfterExit::Leave(RunEnd::Failed)
    );
}

#[test]
fn on_failure_restarts_a_failed_process_and_leaves_one_that_exited_cleanly() {
    let restart = Restart::on_failure(limits(None, Resolve::Never));

    assert_eq!(
        after_exit(&restart, true, 2),
        AfterExit::Restart {
            backoff: limits(None, Resolve::Never).backoff,
            resolve: Resolve::Never,
        }
    );
    assert_eq!(
        after_exit(&restart, false, 0),
        AfterExit::Leave(RunEnd::Completed)
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
        AfterExit::Leave(RunEnd::Failed)
    );
    assert_eq!(
        after_exit(&restart, false, 2),
        AfterExit::Leave(RunEnd::Completed)
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

#[test]
fn a_run_longer_than_a_minute_resets_the_backoff_to_its_first_wait() {
    let backoff = Backoff::new(Duration::from_millis(100), Duration::from_secs(5)).unwrap();

    assert_eq!(streak_after_run(4, HEALTHY_RUN, backoff), 4);
    assert_eq!(
        streak_after_run(4, HEALTHY_RUN + Duration::from_millis(1), backoff),
        0
    );
}

#[test]
fn a_run_must_outlast_a_longest_wait_above_a_minute_to_reset_the_backoff() {
    let backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(120)).unwrap();

    assert_eq!(streak_after_run(3, Duration::from_secs(90), backoff), 3);
    assert_eq!(streak_after_run(3, Duration::from_secs(121), backoff), 0);
}
