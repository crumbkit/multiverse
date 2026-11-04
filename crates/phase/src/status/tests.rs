use std::path::PathBuf;

use super::{ProcessState, SourceSummary, Status, StatusRow, WorkspaceState};
use crate::plan::RunnerKind;
use crate::record::Handle;

fn status() -> Status {
    let mut status = Status::new(
        "reading-list".parse().unwrap(),
        WorkspaceState::PartlyRunning,
    );
    status.rows.push(StatusRow {
        app: "api".parse().unwrap(),
        process: "server".parse().unwrap(),
        runner: RunnerKind::Host,
        source: SourceSummary::Checkout {
            dir: PathBuf::from("/workspaces/reading-list/api"),
        },
        state: ProcessState::Ready,
        handle: Some(Handle::Pid(4242)),
        address: Some("127.0.0.1:41000".parse().unwrap()),
        started_at_ms: Some(1_793_000_000_000),
        expires_at_ms: None,
        restarts: 1,
        log: Some(PathBuf::from("/state/api/server.log")),
    });
    status
}

#[test]
fn a_status_document_survives_a_trip_through_json() {
    let json = serde_json::to_string(&status()).unwrap();
    let read: Status = serde_json::from_str(&json).unwrap();
    assert_eq!(read, status());
}

#[test]
fn a_status_document_names_its_schema_and_the_workspace_state() {
    let json = serde_json::to_value(status()).unwrap();
    assert_eq!(json["schema"], "phase/status/v1");
    assert_eq!(json["state"], "partly-running");
    assert_eq!(json["rows"][0]["address"], "127.0.0.1:41000");
}

#[test]
fn a_status_document_of_another_schema_is_refused() {
    let mut json = serde_json::to_value(status()).unwrap();
    json["schema"] = "phase/status/v2".into();
    assert!(serde_json::from_value::<Status>(json).is_err());
}
