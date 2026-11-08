use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::error::Error;
use crate::name::WorkspaceName;
use crate::record::{
    Checkout, ID_RECORD_VERSION, MULTI_CHECKOUT_RECORD_VERSION, Workspace, WorkspaceKind,
};
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
    let mut created = Vec::new();
    for name in ["zeta", "alpha"] {
        let mut workspace = workspace(name);
        assert_eq!(store.create(&mut workspace).unwrap(), Created::Written);
        created.push(workspace);
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
    assert_eq!(read.as_ref(), created.get(1));
}

#[test]
fn creating_a_name_that_exists_leaves_the_first_record() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let mut first = workspace("app");
    store.create(&mut first).unwrap();
    let mut second = workspace("app");
    second.path = PathBuf::from("/elsewhere");
    assert_eq!(store.create(&mut second).unwrap(), Created::Taken);
    assert_eq!(store.read(&second.name).unwrap(), Some(first));
}

#[test]
fn a_store_leaves_no_temporary_file_behind() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    store.create(&mut workspace("app")).unwrap();
    store.update(&mut workspace("app")).unwrap();
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
    store.create(&mut workspace("app")).unwrap();
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
        r#"{"version": 5, "future": true}"#,
    );
    let error = store.read(&"app".parse().unwrap()).unwrap_err();
    assert!(
        matches!(error, Error::NewerRecord { version: 5, .. }),
        "{error:?}"
    );
}

/// A record as mxv wrote it before workspaces of clones, with no `kind`
/// and no `mirror`.
const WORKTREE_RECORD: &str = r#"{
  "version": 1,
  "name": "inbox-ingest",
  "path": "/home/agent/mxv/inbox-ingest",
  "checkouts": [
    {
      "name": "app",
      "backend": "git",
      "source": "/home/agent/src/app",
      "path": "/home/agent/mxv/inbox-ingest/app",
      "branch": "inbox-ingest",
      "base": "refs/remotes/origin/main",
      "commit": "0123456789abcdef0123456789abcdef01234567"
    }
  ],
  "ports": {"web": 41000}
}"#;

#[test]
fn a_record_without_a_kind_reads_as_a_workspace_of_worktrees() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    write(dir.path().join("inbox-ingest.json"), WORKTREE_RECORD);
    let found = store
        .read(&"inbox-ingest".parse().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(found.kind, WorkspaceKind::Worktree);
    assert_eq!(found.version, 1);
    assert_eq!(found.checkouts[0].mirror, None);
    assert_eq!(found.ports.get("web"), Some(&41000));
}

#[test]
fn a_record_of_worktrees_written_again_gains_a_stable_id_and_version_three_and_keeps_no_kind() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    write(dir.path().join("inbox-ingest.json"), WORKTREE_RECORD);
    let mut found = store
        .read(&"inbox-ingest".parse().unwrap())
        .unwrap()
        .unwrap();
    found.ports.insert("api".to_owned(), 41001);
    store.update(&mut found).unwrap();
    let text = fs::read_to_string(dir.path().join("inbox-ingest.json")).unwrap();
    // An older mxv refuses a record of version 3, so it never rewrites the
    // record without the id.
    assert!(text.contains("\"version\": 3"), "{text}");
    assert!(text.contains("\"id\": "), "{text}");
    assert!(!text.contains("kind"), "{text}");
    assert!(!text.contains("mirror"), "{text}");
}

#[test]
fn a_created_record_of_clones_has_a_stable_id_version_three_and_its_kind() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let mut workspace = workspace("app");
    assert_eq!(workspace.id, None);
    assert_eq!(store.create(&mut workspace).unwrap(), Created::Written);
    assert!(workspace.id.is_some());
    let text = fs::read_to_string(dir.path().join("app.json")).unwrap();
    assert!(text.contains("\"version\": 3"), "{text}");
    assert!(text.contains("\"id\": "), "{text}");
    assert!(text.contains("\"kind\": \"clone\""), "{text}");
    assert_eq!(
        store.read(&"app".parse().unwrap()).unwrap(),
        Some(workspace)
    );
}

#[test]
fn a_record_of_clones_of_version_two_without_a_stable_id_still_reads() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let record = WORKTREE_RECORD
        .replace("\"version\": 1,", "\"version\": 2,\n  \"kind\": \"clone\",")
        .replace("inbox-ingest", "app");
    write(dir.path().join("app.json"), &record);
    let found = store.read(&"app".parse().unwrap()).unwrap().unwrap();
    assert_eq!(found.version, 2);
    assert_eq!(found.kind, WorkspaceKind::Clone);
    assert_eq!(found.id, None);
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
    store.create(&mut workspace("reading-list")).unwrap();
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
    store.create(&mut workspace("reading-list")).unwrap();
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
    store.create(&mut triage).unwrap();
    let text = fs::read_to_string(store.path(&triage.name)).unwrap();
    assert!(!text.contains("ports"), "{text}");
    triage.ports.insert("api".to_owned(), 49_152);
    store.update(&mut triage).unwrap();
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

#[test]
fn a_record_without_a_stable_id_reads_with_none_and_its_first_write_gives_it_one_that_later_writes_keep()
 {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    write(dir.path().join("inbox-ingest.json"), WORKTREE_RECORD);
    let name = "inbox-ingest".parse().unwrap();
    let mut found = store.read(&name).unwrap().unwrap();
    assert_eq!(found.id, None);
    store.update(&mut found).unwrap();
    let id = found.id.clone().unwrap();
    assert_eq!(id.as_str().len(), 32);
    let mut again = store.read(&name).unwrap().unwrap();
    assert_eq!(again.id.as_ref(), Some(&id));
    assert_eq!(again.version, ID_RECORD_VERSION);
    store.update(&mut again).unwrap();
    assert_eq!(store.read(&name).unwrap().unwrap().id, Some(id));
}

#[test]
fn a_workspace_keeps_its_stable_id_when_it_moves() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let mut moved = workspace("triage");
    moved.ensure_id().unwrap();
    let id = moved.id.clone();
    store.create(&mut moved).unwrap();
    moved.path = PathBuf::from("/elsewhere/triage");
    store.update(&mut moved).unwrap();
    let found = store.read(&moved.name).unwrap().unwrap();
    assert_eq!(found.path, PathBuf::from("/elsewhere/triage"));
    assert_eq!(found.id, id);
    assert!(!moved.ensure_id().unwrap());
}

#[test]
fn two_workspaces_get_different_stable_ids() {
    let mut first = workspace("first");
    let mut second = workspace("second");
    assert!(first.ensure_id().unwrap());
    assert!(second.ensure_id().unwrap());
    assert_ne!(first.id, second.id);
}

fn with_two_checkouts(name: &str) -> Workspace {
    let mut workspace = workspace(name);
    let mut second = workspace.checkouts[0].clone();
    second.name = "web".to_owned();
    second.source = PathBuf::from("/home/agent/src/web");
    second.path = workspace.path.join("web");
    workspace.checkouts.push(second);
    workspace
}

#[test]
fn a_record_of_a_workspace_with_two_checkouts_has_version_four() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let mut workspace = with_two_checkouts("app");
    assert_eq!(store.create(&mut workspace).unwrap(), Created::Written);
    let text = fs::read_to_string(dir.path().join("app.json")).unwrap();
    // An older mxv refuses a record of version 4, so it never reads the
    // workspace manifest's globs relative to each checkout.
    assert!(text.contains("\"version\": 4"), "{text}");
    let read = store.read(&"app".parse().unwrap()).unwrap().unwrap();
    assert_eq!(read.checkouts.len(), 2);
    assert_eq!(read.version, MULTI_CHECKOUT_RECORD_VERSION);
}

#[test]
fn a_record_with_one_checkout_written_again_keeps_version_three() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let mut workspace = workspace("app");
    store.create(&mut workspace).unwrap();
    store.update(&mut workspace).unwrap();
    let text = fs::read_to_string(dir.path().join("app.json")).unwrap();
    assert!(text.contains("\"version\": 3"), "{text}");
}

#[test]
fn a_record_of_version_five_is_refused_as_newer() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let mut workspace = with_two_checkouts("app");
    store.create(&mut workspace).unwrap();
    let path = dir.path().join("app.json");
    let text = fs::read_to_string(&path)
        .unwrap()
        .replace("\"version\": 4", "\"version\": 5");
    write(&path, text);
    assert!(matches!(
        store.read(&"app".parse().unwrap()),
        Err(Error::NewerRecord { version: 5, .. })
    ));
}

#[test]
fn a_pin_and_a_reclaim_mark_read_back_and_an_older_record_has_neither() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let mut written = workspace("app");
    store.create(&mut written).unwrap();
    let plain = fs::read_to_string(store.path(&written.name)).unwrap();
    assert!(!plain.contains("pinned"), "{plain}");
    assert!(!plain.contains("reclaimed"), "{plain}");
    let read = store.read(&written.name).unwrap().unwrap();
    assert!(!read.pinned);
    assert_eq!(read.reclaimed, None);
    written.pinned = true;
    written.reclaimed = Some(crate::record::Reclaimed {
        processes: vec!["server".to_owned(), "worker".to_owned()],
    });
    store.update(&mut written).unwrap();
    let read = store.read(&written.name).unwrap().unwrap();
    assert!(read.pinned);
    assert_eq!(
        read.reclaimed.map(|reclaimed| reclaimed.processes),
        Some(vec!["server".to_owned(), "worker".to_owned()])
    );
}
