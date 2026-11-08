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
            AppOutcome::failed("run".parse().unwrap(), StartProblem::exited(Some(2), None)),
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

#[test]
fn an_errored_outcome_carries_the_docker_cause_and_leaves_it_out_of_json_otherwise() {
    use crate::error::PhaseError;
    use crate::record::{DockerCause, DockerError};

    let pull = PhaseError::Docker {
        source: DockerError::refused("manifest unknown".to_owned())
            .with_cause(DockerCause::PullFailed),
    };
    let outcome = AppOutcome::errored(&pull);
    assert!(matches!(
        &outcome,
        AppOutcome::Errored {
            cause: Some(DockerCause::PullFailed),
            ..
        }
    ));
    let json =
        serde_json::to_value(AppReport::new("db".parse().unwrap(), outcome.clone())).unwrap();
    assert_eq!(json["cause"], "pull_failed");
    let back: AppReport = serde_json::from_value(json).unwrap();
    assert_eq!(back.outcome, outcome);

    let busy = PhaseError::UnknownTarget {
        target: "db/server".to_owned(),
    };
    let outcome = AppOutcome::errored(&busy);
    assert!(matches!(&outcome, AppOutcome::Errored { cause: None, .. }));
    let json = serde_json::to_value(AppReport::new("db".parse().unwrap(), outcome)).unwrap();
    assert!(json.get("cause").is_none(), "{json}");
}

#[test]
fn an_errored_outcome_carries_the_docker_context_and_leaves_it_out_of_json_otherwise() {
    use crate::error::PhaseError;
    use crate::record::DockerError;

    let line =
        "Error response from daemon: Bind for 127.0.0.1:8080 failed: port is already allocated";
    let up = PhaseError::Docker {
        source: DockerError::refused(format!(
            "docker compose up failed for the container web: {line}"
        ))
        .during("compose up", Some("web"))
        .with_docker_line(Some(line.to_owned())),
    };
    let outcome = AppOutcome::errored(&up);
    let AppOutcome::Errored {
        context: Some(context),
        ..
    } = &outcome
    else {
        panic!("{outcome:?}");
    };
    assert_eq!(context.operation.as_deref(), Some("compose up"));
    assert_eq!(context.service.as_deref(), Some("web"));
    assert_eq!(context.docker_line.as_deref(), Some(line));
    let json =
        serde_json::to_value(AppReport::new("db".parse().unwrap(), outcome.clone())).unwrap();
    assert_eq!(
        json["context"],
        serde_json::json!({
            "operation": "compose up",
            "service": "web",
            "docker_line": line,
        })
    );
    let back: AppReport = serde_json::from_value(json).unwrap();
    assert_eq!(back.outcome, outcome);

    let other = PhaseError::UnknownTarget {
        target: "db/server".to_owned(),
    };
    let outcome = AppOutcome::errored(&other);
    assert!(matches!(
        &outcome,
        AppOutcome::Errored { context: None, .. }
    ));
    let json = serde_json::to_value(AppReport::new("db".parse().unwrap(), outcome)).unwrap();
    assert!(json.get("context").is_none(), "{json}");
}

#[test]
fn an_errored_outcome_names_a_foreign_object_and_reads_a_context_field_that_a_newer_phase_wrote() {
    use crate::error::PhaseError;
    use crate::record::{DockerError, DockerKind};

    let foreign = PhaseError::Docker {
        source: DockerError::refused("the labels do not name its scope".to_owned())
            .about_foreign(DockerKind::Volume, "phase-0123456789abcdef.data"),
    };
    let json = serde_json::to_value(AppReport::new(
        "db".parse().unwrap(),
        AppOutcome::errored(&foreign),
    ))
    .unwrap();
    assert_eq!(
        json["context"],
        serde_json::json!({
            "foreign": { "kind": "volume", "name": "phase-0123456789abcdef.data" },
        })
    );

    let report: AppReport = serde_json::from_value(serde_json::json!({
        "app": "db",
        "outcome": "errored",
        "code": "phase::docker::refused",
        "message": "Docker refused a request of the run: no",
        "context": {
            "operation": "image push",
            "registry": "example",
            "foreign": { "kind": "socket", "name": "phase-0123456789abcdef.sock" },
        },
    }))
    .unwrap();
    let AppOutcome::Errored {
        context: Some(context),
        ..
    } = report.outcome
    else {
        panic!("{report:?}");
    };
    assert_eq!(context.operation.as_deref(), Some("image push"));
    let foreign = context.foreign.unwrap();
    assert_eq!(foreign.kind(), DockerKind::Unknown);
    assert_eq!(foreign.name(), "phase-0123456789abcdef.sock");
}

#[test]
fn an_exit_names_its_status_or_signal_and_points_to_the_log() {
    assert_eq!(
        StartProblem::exited(Some(3), None).to_string(),
        "it exited with status 3; its log holds its output, which the host program's `logs` command shows, such as `mxv logs <app>`"
    );
    assert!(
        StartProblem::exited(None, Some(9))
            .to_string()
            .starts_with("signal 9 ended it;")
    );
}

#[test]
fn a_plan_change_note_names_the_app_and_process_and_why_up_restarted_it() {
    let app = "api".parse().unwrap();
    let changed = super::Note::plan_changed(&app, "server".parse().unwrap());
    let unrecorded = super::Note::plan_unrecorded(&app, "server".parse().unwrap());

    assert_eq!(
        serde_json::to_value(&changed).unwrap(),
        json!({
            "note": "plan-changed",
            "process": "server",
            "message": "restarted api/server because its part of the plan changed",
        })
    );
    assert_eq!(
        serde_json::to_value(&unrecorded).unwrap()["message"],
        "restarted api/server because the run record does not say which part of the plan it started from"
    );
}
