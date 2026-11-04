use std::fs;
use std::path::PathBuf;

use crate::error::Error;
use crate::name::WorkspaceName;
use crate::record::{Checkout, Workspace};
use crate::store::{Created, Store};
use crate::testing::write;

fn workspace(name: &str) -> Workspace {
    let name: WorkspaceName = name.parse().unwrap();
    let path = PathBuf::from("/home/agent/mxv").join(name.as_str());
    let checkout = Checkout::new(
        "app".to_owned(),
        &PathBuf::from("/home/agent/src/app"),
        path.join("app"),
        name.to_string(),
        "refs/remotes/origin/main".to_owned(),
    );
    Workspace::new(name, path, vec![checkout])
}

#[test]
fn a_store_whose_directory_does_not_exist_lists_no_workspaces() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("workspaces"));
    assert!(store.list().unwrap().workspaces.is_empty());
}

#[test]
fn a_created_record_reads_back_and_lists_in_name_order() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("workspaces"));
    for name in ["zeta", "alpha"] {
        assert_eq!(store.create(&workspace(name)).unwrap(), Created::Written);
    }
    let names: Vec<String> = store
        .list()
        .unwrap()
        .workspaces
        .into_iter()
        .map(|workspace| workspace.name.to_string())
        .collect();
    assert_eq!(names, ["alpha", "zeta"]);
    let read = store.read(&"alpha".parse().unwrap()).unwrap();
    assert_eq!(read, Some(workspace("alpha")));
}

#[test]
fn creating_a_name_that_exists_leaves_the_first_record() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    store.create(&workspace("app")).unwrap();
    let mut second = workspace("app");
    second.path = PathBuf::from("/elsewhere");
    assert_eq!(store.create(&second).unwrap(), Created::Taken);
    assert_eq!(store.read(&second.name).unwrap(), Some(workspace("app")));
}

#[test]
fn a_store_leaves_no_temporary_file_behind() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    store.create(&workspace("app")).unwrap();
    store.update(&workspace("app")).unwrap();
    let names: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, ["app.json"]);
}

#[test]
fn deleting_a_record_twice_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let name: WorkspaceName = "app".parse().unwrap();
    store.create(&workspace("app")).unwrap();
    store.delete(&name).unwrap();
    store.delete(&name).unwrap();
    assert_eq!(store.read(&name).unwrap(), None);
}

#[test]
fn a_record_of_a_newer_version_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    write(
        dir.path().join("app.json"),
        r#"{"version": 2, "future": true}"#,
    );
    let error = store.read(&"app".parse().unwrap()).unwrap_err();
    assert!(
        matches!(error, Error::NewerRecord { version: 2, .. }),
        "{error:?}"
    );
}

#[test]
fn a_record_that_is_not_json_is_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    write(dir.path().join("app.json"), "not json");
    let error = store.read(&"app".parse().unwrap()).unwrap_err();
    assert!(matches!(error, Error::CorruptRecord { .. }), "{error:?}");
}

#[test]
fn a_record_whose_name_breaks_the_rules_is_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let text = serde_json::to_string(&workspace("app"))
        .unwrap()
        .replace("\"name\":\"app\",\"path\"", "\"name\":\"a/b\",\"path\"");
    write(dir.path().join("app.json"), text);
    let error = store.read(&"app".parse().unwrap()).unwrap_err();
    assert!(matches!(error, Error::CorruptRecord { .. }), "{error:?}");
}

#[test]
fn listing_reports_a_corrupt_record_and_a_newer_record_and_still_lists_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    store.create(&workspace("reading-list")).unwrap();
    write(dir.path().join("broken.json"), "not json");
    write(
        dir.path().join("future.json"),
        r#"{"version": 99, "name": "future"}"#,
    );
    let records = store.list().unwrap();
    let names: Vec<&str> = records
        .workspaces
        .iter()
        .map(|workspace| workspace.name.as_str())
        .collect();
    assert_eq!(names, ["reading-list"]);
    assert_eq!(records.problems.len(), 2, "{:?}", records.problems);
    assert!(matches!(records.problems[0], Error::CorruptRecord { .. }));
    assert!(matches!(records.problems[1], Error::NewerRecord { .. }));
}

#[test]
fn a_record_that_holds_another_name_than_its_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    store.create(&workspace("reading-list")).unwrap();
    let text = fs::read_to_string(dir.path().join("reading-list.json")).unwrap();
    write(dir.path().join("app.json"), text);
    let name: WorkspaceName = "app".parse().unwrap();
    let error = store.read(&name).unwrap_err();
    assert!(matches!(error, Error::MismatchedRecord { .. }), "{error:?}");
}
