use std::time::Duration;

use serde_json::json;

use super::{AppOutcome, AppReport, Report, StartProblem};

fn report() -> Report {
    Report::new(vec![
        AppReport::new(
            "api".parse().unwrap(),
            AppOutcome::failed(
                "server".parse().unwrap(),
                StartProblem::port_taken("http".parse().unwrap(), vec![4242]),
            ),
        ),
        AppReport::new("auth".parse().unwrap(), AppOutcome::Started),
        AppReport::new(
            "worker".parse().unwrap(),
            AppOutcome::failed(
                "run".parse().unwrap(),
                StartProblem::exited(Some(2), None, vec!["missing config".into()]),
            ),
        ),
        AppReport::new(
            "web".parse().unwrap(),
            AppOutcome::failed(
                "dev".parse().unwrap(),
                StartProblem::not_ready(Duration::from_secs(60)),
            ),
        ),
    ])
}

#[test]
fn a_report_survives_a_trip_through_json() {
    let json = serde_json::to_string(&report()).unwrap();
    assert_eq!(serde_json::from_str::<Report>(&json).unwrap(), report());
}

#[test]
fn a_report_holds_each_outcome_beside_its_app() {
    let json = serde_json::to_value(report()).unwrap();
    assert_eq!(
        json["apps"][0],
        json!({
            "app": "api",
            "outcome": "failed",
            "process": "server",
            "problem": {"kind": "port-taken", "port": "http", "holders": [4242]},
        })
    );
    assert_eq!(
        json["apps"][1],
        json!({"app": "auth", "outcome": "started"})
    );
}

#[test]
fn a_start_problem_writes_its_wait_in_milliseconds() {
    let json = serde_json::to_value(report()).unwrap();
    assert_eq!(json["apps"][3]["problem"]["waited_ms"], 60_000);
}
