use phase::{
    AppName, AppOutcome, AppReport, Note, ProcessName, ProcessState as Phase, Report, RunnerKind,
    StatusRow,
};

use super::{notes_of, ready_of, stale_sandbox};
use crate::runs::ProcessNote;

fn row(state: Phase) -> StatusRow {
    StatusRow::new(
        AppName::try_from("api".to_owned()).unwrap(),
        ProcessName::try_from("main".to_owned()).unwrap(),
        RunnerKind::Host,
        None,
        state,
    )
}

#[test]
fn a_process_is_ready_once_phase_has_seen_it_ready_since_its_last_start() {
    assert_eq!(ready_of(&row(Phase::Ready)), Some(true));
    assert_eq!(ready_of(&row(Phase::Running)), Some(false));
    assert_eq!(ready_of(&row(Phase::Paused)), Some(false));
    assert_eq!(ready_of(&row(Phase::Starting)), Some(false));
    assert_eq!(ready_of(&row(Phase::Restarting)), Some(false));
}

#[test]
fn a_process_that_does_not_run_has_no_readiness() {
    assert_eq!(ready_of(&row(Phase::Stopped)), None);
    assert_eq!(ready_of(&row(Phase::Stopping)), None);
    assert_eq!(ready_of(&row(Phase::Completed)), None);
}

#[test]
fn the_notes_of_a_report_are_the_secret_notes_of_each_app() {
    let db = AppName::try_from("db".to_owned()).unwrap();
    let api = AppName::try_from("api".to_owned()).unwrap();
    let main = || ProcessName::try_from("main".to_owned()).unwrap();
    let report = Report::new(vec![
        AppReport::new(db.clone(), AppOutcome::AlreadyRunning).with_notes(vec![
            Note::secrets_rewritten(&db, main(), 2, 0),
            Note::not_ours(main()),
        ]),
        AppReport::new(api.clone(), AppOutcome::AlreadyRunning).with_notes(vec![
            Note::secrets_kept(&api, main(), "the source could not supply TOKEN"),
        ]),
        AppReport::new(
            AppName::try_from("web".to_owned()).unwrap(),
            AppOutcome::Started,
        ),
    ]);
    let notes = notes_of(&report);
    assert_eq!(
        notes.get("db"),
        Some(&vec![ProcessNote::SecretsRewritten {
            message: "rewrote 2 secret files of db/main".to_owned(),
        }])
    );
    assert_eq!(
        notes.get("api"),
        Some(&vec![ProcessNote::SecretsKept {
            message: "api/main still runs with its earlier secret files: the source could not supply TOKEN".to_owned(),
        }])
    );
    assert_eq!(notes.get("web"), None);
}

/// Returns a running host row whose recorded sandbox state is `sandbox`,
/// as phase's JSON writes it.
fn sandboxed_row(sandbox: serde_json::Value) -> StatusRow {
    let mut row = row(Phase::Ready);
    row.sandbox = Some(serde_json::from_value(sandbox).unwrap());
    row
}

#[test]
fn a_running_process_that_started_without_permissions_it_now_has_gets_a_warning_naming_the_restart()
{
    let row = sandboxed_row(serde_json::json!({
        "kind": "unsandboxed",
        "reason": "its app declares no permissions",
    }));
    let note = stale_sandbox("triage", "api", true, &row).unwrap();
    assert!(note.is_warning());
    assert_eq!(
        note.message(),
        "api still runs unsandboxed, because it was running before its manifest gave it permissions; run `mxv restart -w triage api` to run it under the sandbox"
    );
}

#[test]
fn a_running_sandboxed_process_whose_permissions_were_removed_gets_a_warning_naming_the_restart() {
    let row = sandboxed_row(serde_json::json!({
        "kind": "seatbelt",
        "partial": "the sandbox cannot limit its listening ports to the loopback interface",
    }));
    let note = stale_sandbox("triage", "api", false, &row).unwrap();
    assert!(note.is_warning());
    assert!(
        note.message()
            .contains("run `mxv restart -w triage api` to run it unsandboxed"),
        "{}",
        note.message()
    );
}

#[test]
fn a_running_process_whose_sandbox_state_matches_its_manifest_gets_no_warning() {
    let undeclared = sandboxed_row(serde_json::json!({
        "kind": "unsandboxed",
        "reason": "its app declares no permissions",
    }));
    let seatbelt = sandboxed_row(serde_json::json!({
        "kind": "seatbelt",
        "partial": "the sandbox cannot limit its listening ports to the loopback interface",
    }));
    // A platform without a sandbox runs a process that declares
    // permissions unsandboxed, for another reason.
    let no_sandbox = sandboxed_row(serde_json::json!({
        "kind": "unsandboxed",
        "reason": "this platform has no sandbox",
    }));
    assert_eq!(stale_sandbox("triage", "api", false, &undeclared), None);
    assert_eq!(stale_sandbox("triage", "api", true, &seatbelt), None);
    assert_eq!(stale_sandbox("triage", "api", true, &no_sandbox), None);
    assert_eq!(
        stale_sandbox("triage", "api", true, &row(Phase::Ready)),
        None
    );
}
