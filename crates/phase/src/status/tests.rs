use std::path::PathBuf;

use super::{ProcessState, SourceSummary, Status, StatusRow, WorkspaceState};
use crate::plan::RunnerKind;
use crate::record::Handle;
use crate::source::Source;

fn checkout() -> SourceSummary {
    SourceSummary::from(&Source::checkout(
        PathBuf::from("/workspaces/reading-list/api"),
        None,
    ))
}

fn status() -> Status {
    let mut status = Status::new("0b9f3c8e".parse().unwrap(), WorkspaceState::PartlyRunning);
    status.name = Some("reading-list".parse().unwrap());
    let mut server = StatusRow::new(
        "api".parse().unwrap(),
        "server".parse().unwrap(),
        RunnerKind::Host,
        Some(checkout()),
        ProcessState::Ready,
    );
    server.handle = Some(Handle::pid(4242, 1_793_000_000_000_000));
    server.address = Some("127.0.0.1:41000".parse().unwrap());
    server.started_at_ms = Some(1_793_000_000_000);
    server.restarts = 1;
    server.log = Some(PathBuf::from("/state/api/server.log"));
    status.rows.push(server);
    status.rows.push(StatusRow::new(
        "api".parse().unwrap(),
        "migrate".parse().unwrap(),
        RunnerKind::Host,
        Some(checkout()),
        ProcessState::exited(Some(1)),
    ));
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
fn a_status_row_holds_its_state_and_exit_code_beside_its_other_fields() {
    let json = serde_json::to_value(status()).unwrap();
    assert_eq!(json["rows"][0]["state"], "ready");
    assert_eq!(json["rows"][1]["state"], "exited");
    assert_eq!(json["rows"][1]["code"], 1);
}

#[test]
fn a_status_row_names_a_process_by_its_pid_and_start_time() {
    let json = serde_json::to_value(status()).unwrap();
    assert_eq!(
        json["rows"][0]["handle"],
        serde_json::json!({"kind": "pid", "pid": 4242, "started_at_us": 1_793_000_000_000_000_u64})
    );
}

#[test]
fn a_new_status_row_has_no_handle_address_times_restarts_or_log() {
    let row = StatusRow::new(
        "api".parse().unwrap(),
        "server".parse().unwrap(),
        RunnerKind::Host,
        Some(checkout()),
        ProcessState::Starting,
    );
    assert_eq!(row.handle, None);
    assert_eq!(row.address, None);
    assert_eq!(row.started_at_ms, None);
    assert_eq!(row.expires_at_ms, None);
    assert_eq!(row.restarts, 0);
    assert_eq!(row.log, None);
}

#[test]
fn a_status_document_of_another_schema_is_refused() {
    let mut json = serde_json::to_value(status()).unwrap();
    json["schema"] = "phase/status/v2".into();
    assert!(serde_json::from_value::<Status>(json).is_err());
}

#[test]
fn an_exited_row_names_the_signal_that_ended_its_process_and_leaves_out_an_absent_one() {
    let mut killed = status();
    killed.rows[1].state = ProcessState::killed(9);

    let json = serde_json::to_value(&killed).unwrap();

    assert_eq!(json["rows"][1]["state"], "exited");
    assert_eq!(json["rows"][1]["code"], serde_json::Value::Null);
    assert_eq!(json["rows"][1]["signal"], 9);
    let plain = serde_json::to_value(status()).unwrap();
    assert!(plain["rows"][1].get("signal").is_none());
    assert!(plain["rows"][0].get("last_exit").is_none());
    let read: Status = serde_json::from_value(json).unwrap();
    assert_eq!(read, killed);
}
