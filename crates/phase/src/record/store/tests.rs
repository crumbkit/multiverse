use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::{Journal, RunStore};
use crate::error::PhaseError;
use crate::name::WorkspaceId;
use crate::record::run::{EntryState, HostProcess, ObjectKey, PlanHash, RunRecord, Started};
use crate::record::testing::{app, empty_record, process, workspace, write_file};

const SHORT: Duration = Duration::from_millis(50);

fn planned() -> RunRecord {
    empty_record().host_process(&app("api"), &process("server"))
}

fn server() -> ObjectKey {
    ObjectKey::Process(app("api"), process("server"))
}

fn host() -> HostProcess {
    HostProcess {
        pid: 4242,
        started_at_us: 1_793_000_000_000_000,
        group: 4242,
    }
}

fn on_disk(store: &RunStore) -> RunRecord {
    store.read(&workspace()).unwrap().unwrap()
}

#[test]
fn a_planned_run_is_on_disk_before_anything_starts() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();

    journal.plan(planned()).unwrap();

    let record = on_disk(&store);
    assert_eq!(record, planned());
    assert_eq!(record.state(&server()), Some(EntryState::Planned));
}

#[test]
fn each_step_of_an_objects_life_is_written_before_the_next() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();

    journal.started(&server(), Started::Host(host())).unwrap();
    assert_eq!(on_disk(&store).state(&server()), Some(EntryState::Running));
    assert_eq!(on_disk(&store).processes[0].host, Some(host()));

    journal.stopping(&server()).unwrap();
    assert_eq!(on_disk(&store).state(&server()), Some(EntryState::Stopping));

    journal.stopped(&server()).unwrap();
    assert_eq!(on_disk(&store).state(&server()), Some(EntryState::Stopped));
}

#[test]
fn the_record_is_readable_only_by_its_owner_and_leaves_no_temporary_file() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();

    journal.plan(planned()).unwrap();

    let path = state.path().join("runs/0b9f3c8e.json");
    assert_eq!(store.path(&workspace()), path);
    let mode = fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let names: Vec<String> = fs::read_dir(state.path().join("runs"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().all(|name| !name.ends_with(".tmp")),
        "{names:?}"
    );
}

#[test]
fn the_record_holds_the_schema_and_no_environment() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    journal.started(&server(), Started::Host(host())).unwrap();

    let text = fs::read_to_string(store.path(&workspace())).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();

    assert_eq!(json["schema"], "phase/run/v1");
    assert_eq!(json["workspace"], "0b9f3c8e");
    assert_eq!(
        json["processes"][0]["host"]["started_at_us"],
        1_793_000_000_000_000_u64
    );
    assert!(!text.contains("env"));
}

#[test]
fn planning_again_keeps_the_state_of_objects_that_are_running() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    journal.started(&server(), Started::Host(host())).unwrap();

    let again = RunRecord::new(workspace(), PlanHash::new([9; 32]))
        .host_process(&app("api"), &process("server"))
        .host_process(&app("web"), &process("server"));
    journal.plan(again).unwrap();

    let record = on_disk(&store);
    assert_eq!(record.plan_hash, PlanHash::new([9; 32]));
    assert_eq!(record.state(&server()), Some(EntryState::Running));
    let web = ObjectKey::Process(app("web"), process("server"));
    assert_eq!(record.state(&web), Some(EntryState::Planned));
}

#[test]
fn a_change_to_an_object_the_record_does_not_name_is_refused() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();

    let other = ObjectKey::Process(app("web"), process("server"));
    let error = journal.started(&other, Started::Host(host())).unwrap_err();

    assert_eq!(error.code(), "phase::record::unplanned");
    let wrong_kind = journal.started(&server(), Started::Volume).unwrap_err();
    assert_eq!(wrong_kind.code(), "phase::record::unplanned");
}

#[test]
fn an_unreadable_record_blocks_only_its_own_workspace() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    drop(journal);
    let broken: WorkspaceId = "broken".parse().unwrap();
    write_file(&store.path(&broken), "{ not json");

    let error = store.read(&broken).unwrap_err();

    assert_eq!(error.code(), "phase::record::unreadable");
    assert!(matches!(error, PhaseError::RecordUnreadable { .. }));
    assert_eq!(on_disk(&store), planned());
    let opened = Journal::open(&store, &broken, SHORT).unwrap_err();
    assert_eq!(opened.code(), "phase::record::unreadable");
}

#[test]
fn a_record_of_another_workspace_under_this_name_is_unreadable() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    let other: WorkspaceId = "other".parse().unwrap();
    let text = fs::read_to_string(store.path(&workspace())).unwrap();
    write_file(&store.path(&other), &text);

    assert_eq!(
        store.read(&other).unwrap_err().code(),
        "phase::record::unreadable"
    );
}

#[test]
fn a_record_from_a_newer_phase_asks_for_an_upgrade() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    fs::create_dir_all(state.path().join("runs")).unwrap();
    write_file(
        &store.path(&workspace()),
        r#"{"schema":"phase/run/v2","something":"new"}"#,
    );

    let error = store.read(&workspace()).unwrap_err();

    assert_eq!(error.code(), "phase::record::newer");
    assert!(error.to_string().contains("upgrade phase"), "{error}");
}

#[test]
fn a_second_lifecycle_lock_waits_and_then_reports_busy() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let _held = store.lock(&workspace(), SHORT).unwrap();

    let began = Instant::now();
    let error = store.lock(&workspace(), SHORT).unwrap_err();

    assert!(began.elapsed() >= SHORT);
    assert_eq!(error.code(), "phase::lock::busy");
}

#[test]
fn the_lock_of_one_workspace_leaves_other_workspaces_free() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let _held = store.lock(&workspace(), SHORT).unwrap();

    let other: WorkspaceId = "other".parse().unwrap();
    assert!(store.lock(&other, SHORT).is_ok());
}

#[test]
fn a_waiting_operation_takes_the_lock_once_the_holder_releases_it() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let held = store.lock(&workspace(), SHORT).unwrap();
    let (taken, waiting) = mpsc::channel();

    let waiter = {
        let store = store.clone();
        thread::spawn(move || {
            let lock = store.lock(&workspace(), Duration::from_secs(10));
            taken.send(lock.is_ok()).unwrap();
        })
    };
    assert!(waiting.recv_timeout(Duration::from_millis(200)).is_err());
    drop(held);

    assert!(waiting.recv_timeout(Duration::from_secs(10)).unwrap());
    waiter.join().unwrap();
}

#[test]
fn a_reader_reads_the_record_while_another_operation_holds_the_lock() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();

    assert_eq!(on_disk(&store), planned());
}

#[test]
fn finishing_removes_the_record_only_when_everything_is_stopped() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    journal.stopped(&server()).unwrap();

    assert!(!journal.finish().unwrap());
    let keeper = ObjectKey::Keeper(app("api"), process("server"));
    journal.stopped(&keeper).unwrap();
    assert!(journal.finish().unwrap());
    assert_eq!(store.read(&workspace()).unwrap(), None);
}
