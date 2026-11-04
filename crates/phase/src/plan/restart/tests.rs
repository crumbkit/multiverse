use std::time::Duration;

use super::{Backoff, BadBackoff, Resolve, Restart, RestartLimits};

#[test]
fn a_backoff_refuses_a_zero_first_wait() {
    assert_eq!(
        Backoff::new(Duration::ZERO, Duration::from_secs(1)),
        Err(BadBackoff::ZeroFirst)
    );
}

#[test]
fn a_backoff_refuses_a_first_wait_longer_than_the_longest() {
    assert_eq!(
        Backoff::new(Duration::from_secs(2), Duration::from_secs(1)),
        Err(BadBackoff::FirstAboveMost)
    );
}

#[test]
fn a_backoff_read_from_json_goes_through_its_check() {
    let refused = serde_json::from_str::<Backoff>(r#"{"first_ms":5000,"most_ms":1000}"#);
    assert!(refused.is_err());
}

#[test]
fn a_restart_policy_is_written_with_its_policy_name() {
    let restart = Restart::OnFailure(RestartLimits::default());
    let json = serde_json::to_value(&restart).unwrap();
    assert_eq!(json["policy"], "on-failure");
    assert_eq!(json["resolve"], "when-expired");
    assert_eq!(serde_json::to_value(Restart::No).unwrap()["policy"], "no");
}

#[test]
fn a_crash_restart_resolves_the_environment_again_only_when_a_value_expired_by_default() {
    assert_eq!(RestartLimits::default().resolve, Resolve::WhenExpired);
}

#[test]
fn a_restart_policy_read_from_json_takes_the_limits_it_omits() {
    let read: Restart = serde_json::from_str(r#"{"policy":"on-failure"}"#).unwrap();

    assert_eq!(read, Restart::OnFailure(RestartLimits::default()));
}

#[test]
fn a_restart_policy_refuses_a_field_it_does_not_know() {
    assert!(serde_json::from_str::<Restart>(r#"{"policy":"no","max":3}"#).is_err());
    assert!(serde_json::from_str::<Restart>(r#"{"policy":"always","maximum":3}"#).is_err());
}

#[test]
fn a_restart_policy_writes_its_waits_in_milliseconds() {
    let json = serde_json::to_value(Restart::on_failure(RestartLimits::default())).unwrap();

    assert_eq!(json["backoff"]["first_ms"], 1000);
    assert_eq!(json["backoff"]["most_ms"], 30_000);
}
