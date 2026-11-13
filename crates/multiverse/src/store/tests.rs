use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::archive::ArchiveId;
use crate::error::Error;
use crate::name::WorkspaceName;
use crate::record::{
    Checkout, ID_RECORD_VERSION, MULTI_CHECKOUT_RECORD_VERSION, Workspace, WorkspaceKind,
    WorkspaceState,
};
use crate::store::{Created, Store, lock};
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
fn a_store_does_not_write_back_a_record_that_was_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let mut workspace = workspace("app");
    store.create(&mut workspace).unwrap();
    store.delete(&workspace.name).unwrap();
    let written = store.update(&mut workspace);
    assert!(
        matches!(&written, Err(Error::RecordMissing { name }) if name.as_str() == "app"),
        "{written:?}"
    );
    let left: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(left.is_empty(), "{left:?}");
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

#[test]
fn a_start_of_named_processes_leaves_the_rest_of_a_reclaim_mark() {
    let mark = crate::record::Reclaimed::new(vec!["server".to_owned(), "worker".to_owned()]);
    assert_eq!(
        mark.without(&["server".to_owned()]),
        Some(crate::record::Reclaimed::new(vec!["worker".to_owned()]))
    );
    assert_eq!(
        mark.without(&["server".to_owned(), "worker".to_owned()]),
        None
    );
    assert_eq!(mark.without(&[]), None);
}

/// Returns the record of `name` with its checkout's commit, as `mxv new`
/// leaves it once the checkout is made.
fn made(name: &str) -> Workspace {
    let mut workspace = workspace(name);
    for checkout in &mut workspace.checkouts {
        checkout.commit = Some("1111111111111111111111111111111111111111".to_owned());
    }
    workspace
}

/// Writes the record of `workspace` to `store` and removes its `state`
/// key, as an older mxv that knew no state wrote it.
fn written_without_state(store: &Store, workspace: &mut Workspace) {
    store.create(workspace).unwrap();
    let path = store.path(&workspace.name);
    let mut value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    value.as_object_mut().unwrap().remove("state");
    write(&path, value.to_string());
}

#[test]
fn an_update_refuses_when_the_stored_state_differs_from_the_state_it_holds() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let mut workspace = made("app");
    store.create(&mut workspace).unwrap();
    let mut held = workspace.clone();
    store.advance(&mut workspace, WorkspaceState::Live).unwrap();
    held.pinned = true;
    let written = store.update(&mut held);
    assert!(
        matches!(&written, Err(Error::StateChanged { name }) if name.as_str() == "app"),
        "{written:?}"
    );
    let read = store.read(&workspace.name).unwrap().unwrap();
    assert_eq!(read.state(), WorkspaceState::Live);
    assert!(!read.pinned);
}

#[test]
fn an_update_writes_when_the_stored_state_is_the_state_it_holds() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let mut workspace = made("app");
    store.create(&mut workspace).unwrap();
    store.advance(&mut workspace, WorkspaceState::Live).unwrap();
    workspace.pinned = true;
    store.update(&mut workspace).unwrap();
    let read = store.read(&workspace.name).unwrap().unwrap();
    assert_eq!(read.state(), WorkspaceState::Live);
    assert!(read.pinned);
}

#[test]
fn an_advance_refuses_live_to_creating_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let mut workspace = made("app");
    store.create(&mut workspace).unwrap();
    store.advance(&mut workspace, WorkspaceState::Live).unwrap();
    let before = fs::read(store.path(&workspace.name)).unwrap();
    let refused = store.advance(&mut workspace, WorkspaceState::Creating);
    assert!(
        matches!(
            &refused,
            Err(Error::TransitionRefused {
                from: WorkspaceState::Live,
                to: WorkspaceState::Creating,
                ..
            })
        ),
        "{refused:?}"
    );
    assert_eq!(workspace.state(), WorkspaceState::Live);
    assert_eq!(fs::read(store.path(&workspace.name)).unwrap(), before);
}

#[test]
fn an_advance_admits_creating_to_live_and_writes_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let mut workspace = made("app");
    store.create(&mut workspace).unwrap();
    assert_eq!(
        store.read(&workspace.name).unwrap().unwrap().state(),
        WorkspaceState::Creating
    );
    store.advance(&mut workspace, WorkspaceState::Live).unwrap();
    assert_eq!(workspace.state(), WorkspaceState::Live);
    let text = fs::read_to_string(store.path(&workspace.name)).unwrap();
    assert!(text.contains("\"state\": \"live\""), "{text}");
}

#[test]
fn an_advance_refuses_when_the_stored_state_differs_from_the_state_it_holds() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let mut workspace = made("app");
    store.create(&mut workspace).unwrap();
    let mut held = workspace.clone();
    store.advance(&mut workspace, WorkspaceState::Live).unwrap();
    let refused = store.advance(&mut held, WorkspaceState::Live);
    assert!(
        matches!(&refused, Err(Error::StateChanged { .. })),
        "{refused:?}"
    );
}

#[test]
fn a_clone_record_without_a_state_whose_checkouts_have_no_commit_reads_as_creating() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let mut workspace = workspace("app");
    written_without_state(&store, &mut workspace);
    let read = store.read(&workspace.name).unwrap().unwrap();
    assert_eq!(read.state(), WorkspaceState::Creating);
}

#[test]
fn a_clone_record_without_a_state_whose_checkout_has_a_commit_reads_as_live() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let mut workspace = made("app");
    written_without_state(&store, &mut workspace);
    let read = store.read(&workspace.name).unwrap().unwrap();
    assert_eq!(read.state(), WorkspaceState::Live);
}

#[test]
fn a_clone_record_without_a_state_that_one_checkout_without_a_commit_leaves_reads_as_live() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let mut workspace = with_two_checkouts("app");
    if let Some(first) = workspace.checkouts.first_mut() {
        first.commit = Some("1111111111111111111111111111111111111111".to_owned());
    }
    written_without_state(&store, &mut workspace);
    let read = store.read(&workspace.name).unwrap().unwrap();
    assert_eq!(read.state(), WorkspaceState::Live);
}

#[test]
fn a_worktree_record_without_a_state_reads_as_live() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let mut workspace = workspace("app").of_worktrees();
    written_without_state(&store, &mut workspace);
    let read = store.read(&workspace.name).unwrap().unwrap();
    assert_eq!(read.state(), WorkspaceState::Live);
}

#[test]
fn a_written_record_reads_back_the_same_at_the_same_version() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    for mut workspace in [workspace("app"), made("web"), made("api").of_worktrees()] {
        store.create(&mut workspace).unwrap();
        let read = store.read(&workspace.name).unwrap().unwrap();
        assert_eq!(read, workspace);
        assert_eq!(read.version, ID_RECORD_VERSION);
        if workspace.state() == WorkspaceState::Creating {
            store.advance(&mut workspace, WorkspaceState::Live).unwrap();
        }
        let read = store.read(&workspace.name).unwrap().unwrap();
        assert_eq!(read, workspace);
        assert_eq!(read.version, ID_RECORD_VERSION);
    }
}

/// The archive id that the tests of `retiring` name.
fn archive_id(digit: char) -> ArchiveId {
    ArchiveId::try_from(std::iter::repeat_n(digit, 32).collect::<String>()).unwrap()
}

/// Returns a store in `dir/workspaces`, whose archives lie in
/// `dir/archives`, holding the record of `name` as `mxv new` leaves it:
/// `live`, with a stable id.
fn live(dir: &Path, name: &str) -> (Store, Workspace) {
    let store = Store::new(dir.join("workspaces"));
    let mut workspace = made(name);
    store.create(&mut workspace).unwrap();
    store.advance(&mut workspace, WorkspaceState::Live).unwrap();
    (store, workspace)
}

#[test]
fn an_advance_admits_live_to_retiring_and_writes_the_archive_id() {
    let dir = tempfile::tempdir().unwrap();
    let (store, mut workspace) = live(dir.path(), "app");
    let retiring = WorkspaceState::retiring(archive_id('a'));
    store.advance(&mut workspace, retiring.clone()).unwrap();
    assert_eq!(workspace.state(), retiring);
    let read = store.read(&workspace.name).unwrap().unwrap();
    assert_eq!(read.state(), retiring);
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(store.path(&workspace.name)).unwrap()).unwrap();
    assert_eq!(
        value["state"],
        serde_json::json!({ "retiring": { "archive": "a".repeat(32) } })
    );
}

#[test]
fn an_advance_to_retiring_mints_a_missing_stable_id_even_beside_an_older_unfinished_archive() {
    let dir = tempfile::tempdir().unwrap();
    let (store, mut workspace) = live(dir.path(), "app");
    written_without_id(&store, &workspace);
    write_older_archive(dir.path(), "app", None);
    let mut held = store.read(&workspace.name).unwrap().unwrap();
    assert_eq!(held.id, None);
    store
        .advance(&mut held, WorkspaceState::retiring(archive_id('a')))
        .unwrap();
    workspace = store.read(&workspace.name).unwrap().unwrap();
    assert!(workspace.id.is_some());
    assert_eq!(workspace.id, held.id);
}

#[test]
fn an_advance_from_retiring_into_an_archive_to_the_same_archive_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (store, mut workspace) = live(dir.path(), "app");
    let retiring = WorkspaceState::retiring(archive_id('a'));
    store.advance(&mut workspace, retiring.clone()).unwrap();
    let before = fs::read(store.path(&workspace.name)).unwrap();
    workspace.pinned = true;
    store.advance(&mut workspace, retiring.clone()).unwrap();
    assert_eq!(workspace.state(), retiring);
    assert_eq!(fs::read(store.path(&workspace.name)).unwrap(), before);
}

#[test]
fn an_advance_refuses_retiring_into_one_archive_to_retiring_into_another() {
    let dir = tempfile::tempdir().unwrap();
    let (store, mut workspace) = live(dir.path(), "app");
    let first = WorkspaceState::retiring(archive_id('a'));
    store.advance(&mut workspace, first.clone()).unwrap();
    let before = fs::read(store.path(&workspace.name)).unwrap();
    let refused = store.advance(&mut workspace, WorkspaceState::retiring(archive_id('b')));
    assert!(
        matches!(&refused, Err(Error::TransitionRefused { from, .. }) if *from == first),
        "{refused:?}"
    );
    assert_eq!(workspace.state(), first);
    assert_eq!(fs::read(store.path(&workspace.name)).unwrap(), before);
}

#[test]
fn a_repoint_moves_a_retiring_workspace_to_the_archive_it_names() {
    let dir = tempfile::tempdir().unwrap();
    let (store, mut workspace) = live(dir.path(), "app");
    store
        .advance(&mut workspace, WorkspaceState::retiring(archive_id('a')))
        .unwrap();
    store.repoint(&mut workspace, &archive_id('c')).unwrap();
    let repointed = WorkspaceState::retiring(archive_id('c'));
    assert_eq!(workspace.state(), repointed);
    assert_eq!(
        store.read(&workspace.name).unwrap().unwrap().state(),
        repointed
    );
}

#[test]
fn a_repoint_refuses_a_live_workspace_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (store, mut workspace) = live(dir.path(), "app");
    let before = fs::read(store.path(&workspace.name)).unwrap();
    let refused = store.repoint(&mut workspace, &archive_id('c'));
    assert!(
        matches!(
            &refused,
            Err(Error::TransitionRefused {
                from: WorkspaceState::Live,
                ..
            })
        ),
        "{refused:?}"
    );
    assert_eq!(workspace.state(), WorkspaceState::Live);
    assert_eq!(fs::read(store.path(&workspace.name)).unwrap(), before);
}

#[test]
fn an_update_of_a_retiring_record_keeps_its_state() {
    let dir = tempfile::tempdir().unwrap();
    let (store, mut workspace) = live(dir.path(), "app");
    let retiring = WorkspaceState::retiring(archive_id('a'));
    store.advance(&mut workspace, retiring.clone()).unwrap();
    workspace.pinned = true;
    workspace.ports.insert("web".to_owned(), 31_000);
    store.update(&mut workspace).unwrap();
    let read = store.read(&workspace.name).unwrap().unwrap();
    assert_eq!(read.state(), retiring);
    assert!(read.pinned);
}

/// Rewrites the record of `workspace` without its stable id, at the
/// version that an older mxv gave a record of clones without one.
fn written_without_id(store: &Store, workspace: &Workspace) {
    let path = store.path(&workspace.name);
    let mut value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let record = value.as_object_mut().unwrap();
    record.remove("id");
    if workspace.checkouts.len() == 1 {
        record.insert("version".to_owned(), 2.into());
    }
    write(&path, value.to_string());
}

/// Writes, in `dir/archives`, the record of a retiring archive of `name`
/// that an older mxv wrote, at version 2, with the workspace id
/// `workspace_id`.
fn write_older_archive(dir: &Path, name: &str, workspace_id: Option<&str>) {
    write_archive(dir, name, 2, workspace_id);
}

/// Writes, in `dir/archives`, the record of a retiring archive of `name` at
/// `version` with the workspace id `workspace_id`.
fn write_archive(dir: &Path, name: &str, version: u32, workspace_id: Option<&str>) {
    let mut record = serde_json::json!({
        "version": version,
        "id": "a".repeat(32),
        "state": "retiring",
        "time": 0,
        "name": name,
        "kind": "clone",
        "path": format!("/home/agent/mxv/{name}"),
        "checkouts": [],
    });
    if let Some(id) = workspace_id {
        record["workspace_id"] = id.into();
    }
    write_archive_text(dir, name, &record.to_string());
}

/// Writes `text` as the archive record of `name` in `dir/archives`.
fn write_archive_text(dir: &Path, name: &str, text: &str) {
    let archive = dir.join("archives").join(name);
    fs::create_dir_all(&archive).unwrap();
    write(archive.join("archive.json"), text);
}

#[test]
fn an_update_of_a_record_without_an_id_beside_an_older_retiring_archive_without_one_mints_none() {
    let dir = tempfile::tempdir().unwrap();
    let (store, workspace) = live(dir.path(), "app");
    written_without_id(&store, &workspace);
    write_older_archive(dir.path(), "app", None);
    let mut held = store.read(&workspace.name).unwrap().unwrap();
    held.pinned = true;
    store.update(&mut held).unwrap();
    let read = store.read(&workspace.name).unwrap().unwrap();
    assert!(read.pinned);
    assert_eq!(read.id, None);
    assert_eq!(held.id, None);
    assert_eq!(read.version, 2);
}

#[test]
fn an_update_of_a_record_without_an_id_and_no_archive_mints_one() {
    let dir = tempfile::tempdir().unwrap();
    let (store, workspace) = live(dir.path(), "app");
    written_without_id(&store, &workspace);
    let mut held = store.read(&workspace.name).unwrap().unwrap();
    store.update(&mut held).unwrap();
    let read = store.read(&workspace.name).unwrap().unwrap();
    assert!(read.id.is_some());
    assert_eq!(read.version, ID_RECORD_VERSION);
}

#[test]
fn an_update_beside_an_older_archive_that_holds_a_workspace_id_or_a_current_one_without_mints_one()
{
    for (version, workspace_id) in [(2, Some("b".repeat(32))), (3, None)] {
        let dir = tempfile::tempdir().unwrap();
        let (store, workspace) = live(dir.path(), "app");
        written_without_id(&store, &workspace);
        write_archive(dir.path(), "app", version, workspace_id.as_deref());
        let mut held = store.read(&workspace.name).unwrap().unwrap();
        store.update(&mut held).unwrap();
        let read = store.read(&workspace.name).unwrap().unwrap();
        assert!(read.id.is_some(), "version {version}");
    }
}

#[test]
fn an_update_of_a_record_without_an_id_beside_an_unreadable_archive_record_mints_none() {
    let dir = tempfile::tempdir().unwrap();
    let (store, workspace) = live(dir.path(), "app");
    written_without_id(&store, &workspace);
    write_archive_text(dir.path(), "app", "not a record\n");
    let mut held = store.read(&workspace.name).unwrap().unwrap();
    store.update(&mut held).unwrap();
    assert_eq!(store.read(&workspace.name).unwrap().unwrap().id, None);
}

#[test]
fn an_update_of_a_record_with_an_id_beside_an_unreadable_archive_record_keeps_its_id() {
    let dir = tempfile::tempdir().unwrap();
    let (store, mut workspace) = live(dir.path(), "app");
    let id = workspace.id.clone();
    write_archive_text(dir.path(), "app", "not a record\n");
    workspace.pinned = true;
    store.update(&mut workspace).unwrap();
    let read = store.read(&workspace.name).unwrap().unwrap();
    assert_eq!(read.id, id);
    assert!(read.pinned);
}

#[test]
fn an_update_that_mints_no_id_raises_two_checkouts_to_version_four_and_never_one_to_three() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("workspaces"));
    let mut workspace = with_two_checkouts("app");
    for checkout in &mut workspace.checkouts {
        checkout.commit = Some("1111111111111111111111111111111111111111".to_owned());
    }
    store.create(&mut workspace).unwrap();
    store.advance(&mut workspace, WorkspaceState::Live).unwrap();
    written_without_id(&store, &workspace);
    let path = store.path(&workspace.name);
    let text = fs::read_to_string(&path)
        .unwrap()
        .replace("\"version\":4", "\"version\":2");
    write(&path, text);
    write_older_archive(dir.path(), "app", None);
    let mut held = store.read(&workspace.name).unwrap().unwrap();
    assert_eq!(held.version, 2);
    store.update(&mut held).unwrap();
    let read = store.read(&workspace.name).unwrap().unwrap();
    assert_eq!(read.id, None);
    assert_eq!(read.version, MULTI_CHECKOUT_RECORD_VERSION);
}
