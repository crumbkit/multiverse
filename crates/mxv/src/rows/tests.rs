use std::path::PathBuf;

use multiverse::health::{Problem, Repair};
use multiverse::{ProcessNote, Workspace};

use super::{
    KindName, NewRow, NoteRow, ProcessRow, ShowRow, StateName, WorkspaceRow, repair_text,
    text_of_problem,
};

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

/// Returns a workspace `reading-list` with no checkouts and the id `id`.
fn workspace(id: Option<&str>) -> Workspace {
    let mut workspace = Workspace::new(
        "reading-list".parse().unwrap(),
        PathBuf::from("/home/agent/mxv/reading-list"),
        Vec::new(),
    );
    workspace.id = id.map(|id| id.parse().unwrap());
    workspace
}

#[test]
fn a_workspace_row_without_an_id_serialises_a_null_id() {
    let json = serde_json::to_value(WorkspaceRow::from(&workspace(None))).unwrap();
    assert_eq!(json["id"], serde_json::Value::Null, "{json}");
}

#[test]
fn a_workspace_row_serialises_the_id_that_its_record_holds() {
    let id = "0123456789abcdef0123456789abcdef";
    let json = serde_json::to_value(WorkspaceRow::from(&workspace(Some(id)))).unwrap();
    assert_eq!(json["id"], id, "{json}");
}

#[test]
fn the_id_is_a_long_column_of_each_workspace_row_type() {
    use crumbkit::Row as _;
    for (row, columns, long) in [
        ("list", WorkspaceRow::COLUMNS, WorkspaceRow::LONG_COLUMNS),
        ("show", ShowRow::COLUMNS, ShowRow::LONG_COLUMNS),
        ("new", NewRow::COLUMNS, NewRow::LONG_COLUMNS),
    ] {
        assert!(!columns.contains(&"id"), "{row} prints id by default");
        assert!(long.contains(&"id"), "{row} has no long id column");
    }
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
    // Each row's first two cells, the problem and the repair, trimmed. A
    // problem matches only a whole cell, so a text that starts another
    // problem's text does not stand in for it.
    let cells: Vec<Vec<&str>> = rows
        .iter()
        .map(|row| {
            row.split(" | ")
                .map(|cell| cell.trim_matches(['|', ' ']))
                .take(2)
                .collect()
        })
        .collect();
    let missing: Vec<String> = every_problem()
        .iter()
        .filter_map(|problem| {
            let (column, text) = match problem {
                // The problem of a record that cannot be read is the read
                // error, so its row is found by its repair.
                Problem::Unreadable { .. } => (
                    1,
                    format!(
                        "`{}`",
                        repair_text(&Repair::MoveAside(PathBuf::from("PATH")))
                    ),
                ),
                problem => (0, text_of_problem(problem, "NAME")),
            };
            let found = cells
                .iter()
                .any(|row| row.get(column).is_some_and(|cell| *cell == text));
            (!found).then_some(text)
        })
        .collect();
    assert!(
        missing.is_empty(),
        "the table under ### Problems and their repairs in crates/mxv/README.md has no row for {missing:?}"
    );
}
