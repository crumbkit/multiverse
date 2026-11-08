use crumbkit::Row as _;
use multiverse::ProcessNote;

use super::{KindName, NoteRow, ProcessRow, StateName};

fn row(notes: &[ProcessNote]) -> ProcessRow {
    ProcessRow {
        name: "db".to_owned(),
        kind: KindName::Container,
        state: StateName::Running,
        ready: Some(true),
        pid: None,
        code: None,
        signal: None,
        reason: None,
        ports: Vec::new(),
        log: None,
        sandbox: None,
        notes: notes.iter().map(NoteRow::from).collect(),
    }
}

#[test]
fn kept_secrets_print_as_a_warning_and_rewritten_secrets_as_an_info_line() {
    let row = row(&[
        ProcessNote::SecretsKept {
            message: "db/main still runs with its earlier secret files: the source failed"
                .to_owned(),
        },
        ProcessNote::SecretsRewritten {
            message: "rewrote 2 secret files of db/main".to_owned(),
        },
    ]);
    assert_eq!(
        row.child_lines(),
        [
            "warning: db/main still runs with its earlier secret files: the source failed",
            "info: rewrote 2 secret files of db/main",
        ]
    );
    let json = serde_json::to_value(&row).unwrap();
    assert_eq!(json["notes"][0]["level"], "warning");
    assert_eq!(json["notes"][1]["level"], "info");
    assert_eq!(
        json["notes"][1]["message"],
        "rewrote 2 secret files of db/main"
    );
}

#[test]
fn a_process_without_notes_prints_no_child_lines() {
    assert!(row(&[]).child_lines().is_empty());
}
