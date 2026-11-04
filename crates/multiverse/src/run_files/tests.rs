use std::path::Path;

use crate::name::WorkspaceName;
use crate::run_files::RunDir;

#[test]
fn a_run_directory_names_each_units_files_logs_and_the_workspaces_lock() {
    let dir = RunDir::new(
        Path::new("/state/runs"),
        &WorkspaceName::try_from("reading-list".to_owned()).unwrap(),
    );
    assert_eq!(dir.path(), Path::new("/state/runs/reading-list"));
    assert_eq!(
        dir.file("db", "container"),
        Path::new("/state/runs/reading-list/db.container")
    );
    assert_eq!(dir.log("db"), Path::new("/state/runs/reading-list/db.log"));
    assert_eq!(
        dir.older_log("db"),
        Path::new("/state/runs/reading-list/db.log.1")
    );
    assert_eq!(dir.lock(), Path::new("/state/runs/reading-list/.lock"));
}
