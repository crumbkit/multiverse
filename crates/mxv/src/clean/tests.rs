use std::time::{Duration, SystemTime};

use super::{DEFAULT_IDLE, busy, over_limit};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

fn days_ago(now: SystemTime, days: u32) -> Option<SystemTime> {
    Some(now - DAY * days)
}

#[test]
fn a_workspace_whose_processes_run_is_never_idle() {
    let now = SystemTime::now();
    let reason = busy(&["web".to_owned()], days_ago(now, 30), now, DEFAULT_IDLE).unwrap();
    assert!(reason.contains("web"), "{reason}");
}

#[test]
fn a_workspace_changed_within_the_idle_age_is_not_idle() {
    let now = SystemTime::now();
    let reason = busy(&[], days_ago(now, 2), now, DEFAULT_IDLE).unwrap();
    assert!(reason.contains("2d ago"), "{reason}");
    assert!(reason.contains("1w"), "{reason}");
}

#[test]
fn a_workspace_unchanged_for_the_idle_age_or_never_changed_is_idle() {
    let now = SystemTime::now();
    assert_eq!(busy(&[], days_ago(now, 8), now, DEFAULT_IDLE), None);
    assert_eq!(busy(&[], None, now, DEFAULT_IDLE), None);
    assert_eq!(busy(&[], Some(now), now, Duration::ZERO), None);
}

#[test]
fn over_the_limit_the_oldest_idle_workspaces_are_cleaned_until_the_total_fits() {
    let now = SystemTime::now();
    let candidates = [
        (days_ago(now, 10), 30, true),
        (days_ago(now, 40), 20, true),
        (days_ago(now, 1), 50, false),
        (days_ago(now, 20), 25, true),
    ];
    // 125 bytes in all against a limit of 80: the oldest, 20 bytes, leaves
    // 105, and the next oldest, 25 bytes, leaves 80.
    assert_eq!(over_limit(&candidates, 125, 80), [1, 3]);
}

#[test]
fn under_the_limit_nothing_is_cleaned() {
    let now = SystemTime::now();
    let candidates = [(days_ago(now, 10), 30, true)];
    assert!(over_limit(&candidates, 30, 30).is_empty());
}

#[test]
fn over_the_limit_a_busy_or_empty_workspace_is_never_cleaned() {
    let now = SystemTime::now();
    let candidates = [(days_ago(now, 50), 90, false), (days_ago(now, 40), 0, true)];
    assert!(over_limit(&candidates, 90, 10).is_empty());
}
