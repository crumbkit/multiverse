use multiverse::ProcessNote;
use multiverse::health::Problem;

use super::{KindName, NoteRow, ProcessRow, StateName, text_of_problem};

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

#[test]
fn the_port_row_keeps_only_its_name_uncut() {
    assert_eq!(<super::PortRow as crumbkit::Row>::UNCUT_COLUMNS, &["name"]);
}

/// Returns one value of each [`Problem`]. The `match` fails to compile when
/// a problem is added, until this list holds it.
fn every_problem() -> Vec<Problem> {
    let _ = |problem: &Problem| match problem {
        Problem::Unreadable { .. }
        | Problem::ArchiveUnfinished
        | Problem::ArchiveNotStarted
        | Problem::ArchiveWorkspaceRemoved
        | Problem::ArchivePinsGone
        | Problem::ArchiveBesideNewerWorkspace
        | Problem::RenameUnfinished
        | Problem::RestoreUnfinished
        | Problem::RestoreStopped
        | Problem::CheckoutsGone
        | Problem::CreateUnfinished => {}
    };
    vec![
        Problem::Unreadable {
            reason: String::new(),
        },
        Problem::ArchiveUnfinished,
        Problem::ArchiveNotStarted,
        Problem::ArchiveWorkspaceRemoved,
        Problem::ArchivePinsGone,
        Problem::ArchiveBesideNewerWorkspace,
        Problem::RenameUnfinished,
        Problem::RestoreUnfinished,
        Problem::RestoreStopped,
        Problem::CheckoutsGone,
        Problem::CreateUnfinished,
    ]
}

#[test]
fn the_readme_problem_table_has_a_row_for_every_problem() {
    let readme = include_str!("../../README.md");
    let start = readme
        .find("\n### Problems and their repairs\n")
        .expect("README.md has the problem table's heading");
    let rows: Vec<&str> = readme[start..]
        .lines()
        .skip_while(|line| !line.starts_with('|'))
        .take_while(|line| line.starts_with('|'))
        .skip(2)
        .collect();
    let missing: Vec<String> = every_problem()
        .iter()
        .map(|problem| match problem {
            // The problem of a record that cannot be read is the read
            // error, so its row is found by its repair.
            Problem::Unreadable { .. } => "move PATH aside by hand".to_owned(),
            problem => text_of_problem(problem, "NAME"),
        })
        .filter(|text| !rows.iter().any(|row| row.contains(text.as_str())))
        .collect();
    assert!(
        missing.is_empty(),
        "the table under ### Problems and their repairs in crates/mxv/README.md has no row for {missing:?}"
    );
}
