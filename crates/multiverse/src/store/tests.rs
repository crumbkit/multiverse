use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::error::Error;
use crate::name::WorkspaceName;
use crate::record::{Checkout, Workspace};
use crate::store::{Created, Store, cap_log, lock, rotate_log};
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

#[test]
fn a_record_keeps_its_allocated_ports_and_a_record_without_ports_leaves_the_key_out() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let mut triage = workspace("triage");
    store.create(&triage).unwrap();
    let text = fs::read_to_string(store.path(&triage.name)).unwrap();
    assert!(!text.contains("ports"), "{text}");
    triage.ports.insert("api".to_owned(), 49_152);
    store.update(&triage).unwrap();
    assert_eq!(
        store.read(&triage.name).unwrap().unwrap().ports["api"],
        49_152
    );
}

#[test]
fn a_second_lock_of_a_held_file_waits_and_then_fails_with_busy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runs").join("triage").join(".lock");
    let _held = lock(&path, Duration::ZERO).unwrap();
    let started = Instant::now();
    let second = lock(&path, Duration::from_millis(300));
    assert!(matches!(second, Err(Error::Busy { .. })), "{second:?}");
    assert!(started.elapsed() >= Duration::from_millis(300));
}

#[test]
fn a_dropped_lock_lets_the_next_caller_take_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".lock");
    drop(lock(&path, Duration::ZERO).unwrap());
    assert!(lock(&path, Duration::ZERO).is_ok());
}

#[test]
fn a_log_over_its_cap_is_copied_to_the_older_log_and_emptied_while_its_writer_appends() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("web.log");
    let older = dir.path().join("web.log.1");
    let mut writer = crate::store::append_file(&log).unwrap();
    std::io::Write::write_all(&mut writer, b"0123456789\n").unwrap();
    cap_log(&log, &older, 100).unwrap();
    assert!(!older.exists(), "a log under its cap stays");
    cap_log(&log, &older, 5).unwrap();
    assert_eq!(fs::read_to_string(&older).unwrap(), "0123456789\n");
    std::io::Write::write_all(&mut writer, b"next\n").unwrap();
    assert_eq!(fs::read_to_string(&log).unwrap(), "next\n");
}

#[test]
fn rotating_a_missing_log_succeeds_and_a_present_log_replaces_the_older_one() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("web.log");
    let older = dir.path().join("web.log.1");
    rotate_log(&log, &older).unwrap();
    write(&older, "oldest\n");
    write(&log, "previous\n");
    rotate_log(&log, &older).unwrap();
    assert_eq!(fs::read_to_string(&older).unwrap(), "previous\n");
    assert!(!log.exists());
}
