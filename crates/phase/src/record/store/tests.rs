use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::{Journal, RunStore};
use crate::error::PhaseError;
use crate::name::WorkspaceId;
use crate::plan::restart::{Restart, RestartLimits, Stop, StopSignal};
use crate::record::ContainerId;
use crate::record::run::{
    DependencyRun, Ended, EntryState, Exit, HostProcess, Launching, ObjectKey, OneshotLaunch,
    PlanHash, Ready, RunRecord, Started,
};
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
        boot_id: None,
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

    assert_eq!(json["schema"], "phase/run/v5");
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
        r#"{"schema":"phase/run/v6","something":"new"}"#,
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

fn set_mode(path: &std::path::Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn a_records_directory_that_others_may_read_is_narrowed_to_its_owner() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let runs = state.path().join("runs");
    fs::create_dir(&runs).unwrap();
    set_mode(&runs, 0o755);

    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();

    assert_eq!(
        fs::metadata(&runs).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

#[test]
fn a_records_directory_that_is_a_symbolic_link_is_refused() {
    let state = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), state.path().join("runs")).unwrap();
    let store = RunStore::new(state.path());

    let error = store.lock(&workspace(), SHORT).unwrap_err();

    assert_eq!(error.code(), "phase::state::unwritable");
}

#[test]
fn a_write_never_follows_a_symbolic_link_left_where_its_temporary_file_goes() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let lock = store.lock(&workspace(), SHORT).unwrap();
    let target = state.path().join("target");
    write_file(&target, "untouched");
    let temp = state
        .path()
        .join(format!("runs/.0b9f3c8e.json.{}.tmp", std::process::id()));
    std::os::unix::fs::symlink(&target, &temp).unwrap();

    store.write(&planned(), &lock).unwrap();

    assert_eq!(fs::read_to_string(&target).unwrap(), "untouched");
    assert_eq!(on_disk(&store), planned());
    assert!(!temp.exists());
}

#[test]
fn a_directory_that_cannot_be_synced_fails_a_write_and_a_removal() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let lock = store.lock(&workspace(), SHORT).unwrap();
    let runs = state.path().join("runs");
    // Without read permission the directory cannot be opened to sync it,
    // while files in it can still be created, renamed and removed.
    set_mode(&runs, 0o300);

    let written = store.write(&planned(), &lock);
    let removed = store.remove(&workspace(), &lock);
    set_mode(&runs, 0o700);

    assert_eq!(written.unwrap_err().code(), "phase::state::unwritable");
    assert_eq!(removed.unwrap_err().code(), "phase::state::unwritable");
}

#[test]
fn a_record_that_names_pid_1_or_group_0_is_unreadable() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    journal.started(&server(), Started::Host(host())).unwrap();
    drop(journal);
    let text = fs::read_to_string(store.path(&workspace())).unwrap();

    for (from, to) in [
        ("\"pid\": 4242", "\"pid\": 1"),
        ("\"group\": 4242", "\"group\": 0"),
    ] {
        assert!(text.contains(from), "{text}");
        write_file(&store.path(&workspace()), &text.replace(from, to));

        let error = store.read(&workspace()).unwrap_err();

        assert_eq!(error.code(), "phase::record::unreadable", "{to}");
    }
}

#[test]
fn a_restart_records_the_new_process_and_the_count_of_restarts() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    journal.started(&server(), Started::Host(host())).unwrap();
    assert_eq!(on_disk(&store).processes[0].restarts, 0);
    let again = HostProcess {
        pid: 4343,
        group: 4343,
        ..host()
    };

    journal.restarted(&server(), again.clone(), 3).unwrap();

    let record = on_disk(&store);
    assert_eq!(record.state(&server()), Some(EntryState::Running));
    assert_eq!(record.processes[0].host, Some(again));
    assert_eq!(record.processes[0].restarts, 3);
}

#[test]
fn a_record_without_restart_counts_reads_as_no_restarts() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    drop(journal);
    let path = store.path(&workspace());
    let text = fs::read_to_string(&path).unwrap();
    let mut json: serde_json::Value = serde_json::from_str(&text).unwrap();
    json["processes"][0]
        .as_object_mut()
        .unwrap()
        .remove("restarts");
    write_file(&path, &json.to_string());

    assert_eq!(on_disk(&store).processes[0].restarts, 0);
}

fn launching(grace_ms: u64, hash: u8) -> Launching {
    Launching {
        plan_hash: PlanHash::new([hash; 32]),
        stop: Stop {
            signal: StopSignal::Int,
            grace: Duration::from_millis(grace_ms),
        },
        listen: None,
        restart: Restart::No,
        sandbox: Some(crate::sandbox::Sandboxing::seatbelt()),
        oneshot: None,
    }
}

/// What phase records when it launches a oneshot from the part of the
/// plan with the hash `[7; 32]`, after a database container and a cache
/// process that it depends on.
fn oneshot_launch() -> OneshotLaunch {
    OneshotLaunch {
        plan_hash: PlanHash::new([7; 32]),
        depends: Some(vec![
            DependencyRun::Host {
                app: app("cache"),
                process: process("server"),
                keeper: host(),
            },
            DependencyRun::Container {
                app: app("db"),
                process: process("postgres"),
                id: ContainerId::try_from("c0".repeat(32)).unwrap(),
                started_at: "2026-11-05T09:00:00.123456789Z".to_owned(),
            },
        ]),
    }
}

/// Opens a journal of `store` whose record holds the oneshot `api/server`,
/// launched as [`oneshot_launch`] says and started.
fn launched_oneshot(store: &RunStore) -> Journal<'_> {
    let mut journal = Journal::open(store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    journal
        .launching(
            &app("api"),
            &process("server"),
            &Launching {
                oneshot: Some(oneshot_launch()),
                ..launching(300, 1)
            },
        )
        .unwrap();
    journal.started(&server(), Started::Host(host())).unwrap();
    journal
}

#[test]
fn launching_a_process_again_writes_its_entries_back_to_planned_with_no_host() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    let keeper = ObjectKey::Keeper(app("api"), process("server"));
    journal.started(&keeper, Started::Host(host())).unwrap();
    journal.started(&server(), Started::Host(host())).unwrap();
    journal.restarted(&server(), host(), 2).unwrap();
    journal.stopped(&server()).unwrap();

    journal
        .launching(&app("api"), &process("server"), &launching(300, 9))
        .unwrap();

    let record = on_disk(&store);
    for entry in [&record.keepers[0], &record.processes[0]] {
        assert_eq!(entry.state, EntryState::Planned);
        assert_eq!(entry.host, None);
        assert_eq!(entry.restarts, 0);
    }
    assert_eq!(record.processes[0].stop, Some(launching(300, 9).stop));
    assert_eq!(record.processes[0].plan_hash, Some(PlanHash::new([9; 32])));
}

#[test]
fn planning_again_keeps_how_a_known_process_stops_and_the_plan_it_started_from() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    journal
        .launching(&app("api"), &process("server"), &launching(300, 1))
        .unwrap();
    journal.started(&server(), Started::Host(host())).unwrap();

    let again = RunRecord::new(workspace(), PlanHash::new([2; 32])).planned_host_process(
        &app("api"),
        &process("server"),
        Some(launching(900, 2).stop),
        None,
    );
    journal.plan(again).unwrap();

    let record = on_disk(&store);
    assert_eq!(record.processes[0].stop, Some(launching(300, 1).stop));
    assert_eq!(record.processes[0].plan_hash, Some(PlanHash::new([1; 32])));
}

#[test]
fn a_fresh_start_of_a_process_counts_its_restarts_from_zero() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    journal.started(&server(), Started::Host(host())).unwrap();
    journal.restarted(&server(), host(), 3).unwrap();

    journal.started(&server(), Started::Host(host())).unwrap();

    assert_eq!(on_disk(&store).processes[0].restarts, 0);
}

#[test]
fn a_write_whose_directory_sync_fails_after_the_rename_keeps_the_new_record_in_the_journal() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    let runs = state.path().join("runs");
    set_mode(&runs, 0o300);

    let started = journal.started(&server(), Started::Host(host()));
    set_mode(&runs, 0o700);

    assert_eq!(started.unwrap_err().code(), "phase::state::unwritable");
    assert_eq!(on_disk(&store).state(&server()), Some(EntryState::Running));
    assert_eq!(
        journal.record().unwrap().state(&server()),
        Some(EntryState::Running)
    );
}

fn crashed(code: i32) -> Ended {
    Ended {
        exit: Some(Exit::Code(code)),
        requested: false,
    }
}

#[test]
fn a_record_without_an_end_reads_as_a_process_that_has_not_ended() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    journal.started(&server(), Started::Host(host())).unwrap();
    drop(journal);

    let text = fs::read_to_string(store.path(&workspace())).unwrap();

    assert!(!text.contains("ended"), "{text}");
    assert_eq!(on_disk(&store).processes[0].ended, None);
}

#[test]
fn an_end_is_recorded_with_its_exit_and_whether_a_stop_request_caused_it() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    journal.started(&server(), Started::Host(host())).unwrap();

    journal.ending(&server(), crashed(3)).unwrap();
    assert_eq!(on_disk(&store).state(&server()), Some(EntryState::Stopping));
    let killed = Ended {
        exit: Some(Exit::Signal(9)),
        requested: false,
    };
    journal.stopped_after(&server(), killed).unwrap();

    let record = on_disk(&store);
    assert_eq!(record.state(&server()), Some(EntryState::Stopped));
    assert_eq!(record.processes[0].ended, Some(killed));
    let text = fs::read_to_string(store.path(&workspace())).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        json["processes"][0]["ended"],
        serde_json::json!({"exit": {"signal": 9}, "requested": false})
    );
}

#[test]
fn a_restart_keeps_the_last_end_and_a_fresh_start_or_launch_clears_it() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    journal.started(&server(), Started::Host(host())).unwrap();
    journal.stopped_after(&server(), crashed(3)).unwrap();

    journal.restarted(&server(), host(), 1).unwrap();
    assert_eq!(on_disk(&store).processes[0].ended, Some(crashed(3)));

    journal.started(&server(), Started::Host(host())).unwrap();
    assert_eq!(on_disk(&store).processes[0].ended, None);

    journal.stopped_after(&server(), crashed(4)).unwrap();
    journal
        .launching(&app("api"), &process("server"), &launching(300, 1))
        .unwrap();
    assert_eq!(on_disk(&store).processes[0].ended, None);
}

#[test]
fn a_stop_that_phase_begins_records_the_end_as_requested() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    journal.started(&server(), Started::Host(host())).unwrap();
    journal.stopped_after(&server(), crashed(3)).unwrap();
    journal.restarted(&server(), host(), 1).unwrap();

    journal.stopping(&server()).unwrap();
    journal.stopped(&server()).unwrap();

    let ended = on_disk(&store).processes[0].ended;
    assert_eq!(
        ended,
        Some(Ended {
            exit: None,
            requested: true
        })
    );
    let keeper = ObjectKey::Keeper(app("api"), process("server"));
    journal.stopping(&keeper).unwrap();
    assert_eq!(on_disk(&store).keepers[0].ended, None);
}

#[test]
fn a_record_that_an_older_phase_wrote_reads_and_is_written_back_in_the_new_version() {
    for older in [
        "phase/run/v1",
        "phase/run/v2",
        "phase/run/v3",
        "phase/run/v4",
    ] {
        let state = tempfile::tempdir().unwrap();
        let store = RunStore::new(state.path());
        let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
        journal.plan(planned()).unwrap();
        drop(journal);
        let path = store.path(&workspace());
        let text = fs::read_to_string(&path).unwrap();
        write_file(&path, &text.replace("phase/run/v5", older));

        let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
        assert_eq!(journal.record(), Some(&planned()), "{older}");
        journal.started(&server(), Started::Host(host())).unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json["schema"], "phase/run/v5", "{older}");
    }
}

#[test]
fn a_record_that_an_older_phase_wrote_reads_as_one_that_has_observed_no_readiness() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal
        .plan(planned().container(
            &app("db"),
            &process("server"),
            "mxv-0b9f3c8e-db-server-1".parse().unwrap(),
        ))
        .unwrap();
    journal.started(&server(), Started::Host(host())).unwrap();
    drop(journal);
    let path = store.path(&workspace());
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains("ready"), "{text}");
    write_file(&path, &text.replace("phase/run/v5", "phase/run/v2"));

    let record = on_disk(&store);

    assert_eq!(record.processes[0].ready, None);
    assert_eq!(record.containers[0].ready, None);
}

#[test]
fn readiness_is_recorded_with_its_time_and_the_restarts_it_saw() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    journal.started(&server(), Started::Host(host())).unwrap();

    journal
        .ready(&server(), Ready::new(1_793_000_000_123, 0))
        .unwrap();

    assert_eq!(
        on_disk(&store).processes[0].ready,
        Some(Ready::new(1_793_000_000_123, 0))
    );
    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(store.path(&workspace())).unwrap()).unwrap();
    assert_eq!(
        json["processes"][0]["ready"],
        serde_json::json!({"at_ms": 1_793_000_000_123_u64, "restarts": 0})
    );
}

#[test]
fn a_restart_clears_readiness() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    journal.started(&server(), Started::Host(host())).unwrap();
    journal.ready(&server(), Ready::new(100, 0)).unwrap();

    journal.restarted(&server(), host(), 1).unwrap();

    assert_eq!(on_disk(&store).processes[0].ready, None);
}

#[test]
fn a_start_or_a_launch_by_phase_clears_readiness() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    journal.started(&server(), Started::Host(host())).unwrap();
    journal.ready(&server(), Ready::new(100, 0)).unwrap();

    journal
        .launching(&app("api"), &process("server"), &launching(300, 1))
        .unwrap();
    assert_eq!(on_disk(&store).processes[0].ready, None);

    journal.ready(&server(), Ready::new(100, 0)).unwrap();
    journal.started(&server(), Started::Host(host())).unwrap();
    assert_eq!(on_disk(&store).processes[0].ready, None);
}

#[test]
fn a_container_records_readiness_until_phase_starts_it_again() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal
        .plan(empty_record().container(
            &app("db"),
            &process("server"),
            "mxv-0b9f3c8e-db-server-1".parse().unwrap(),
        ))
        .unwrap();
    let key = ObjectKey::Container(app("db"), process("server"));
    let id = || ContainerId::try_from("a".repeat(64)).unwrap();
    let hash = || PlanHash::new([7; 32]);
    journal
        .started(&key, Started::Container(id(), hash()))
        .unwrap();

    let ready = Ready::new(100, 1).with_container_start("2026-11-05T10:20:30Z".to_owned());
    journal.ready(&key, ready.clone()).unwrap();
    assert_eq!(on_disk(&store).containers[0].ready, Some(ready));
    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(store.path(&workspace())).unwrap()).unwrap();
    assert_eq!(
        json["containers"][0]["ready"]["container_started_at"],
        "2026-11-05T10:20:30Z"
    );

    journal
        .containers_launching(&app("db"), &[process("server")])
        .unwrap();
    assert_eq!(on_disk(&store).containers[0].ready, None);

    journal.ready(&key, Ready::new(100, 1)).unwrap();
    journal
        .started(&key, Started::Container(id(), hash()))
        .unwrap();
    assert_eq!(on_disk(&store).containers[0].ready, None);
}

#[test]
fn readiness_of_a_keeper_or_a_network_is_unplanned() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();

    let keeper = ObjectKey::Keeper(app("api"), process("server"));
    let error = journal.ready(&keeper, Ready::new(100, 0)).unwrap_err();

    assert_eq!(error.code(), "phase::record::unplanned");
}

#[test]
fn a_stop_that_phase_begins_keeps_the_end_of_a_process_that_already_ended_by_itself() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    journal.started(&server(), Started::Host(host())).unwrap();
    journal.ending(&server(), crashed(3)).unwrap();

    journal.stopping(&server()).unwrap();
    journal.stopped(&server()).unwrap();

    assert_eq!(on_disk(&store).processes[0].ended, Some(crashed(3)));
}

#[test]
fn launching_a_process_records_its_restart_policy() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    let mut always = launching(300, 1);
    always.restart = Restart::always(RestartLimits::default());

    journal
        .launching(&app("api"), &process("server"), &always)
        .unwrap();

    assert_eq!(
        on_disk(&store).processes[0].restart,
        Some(Restart::always(RestartLimits::default()))
    );
}

#[test]
fn a_v3_record_reads_as_one_that_recorded_no_sandbox() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    drop(journal);
    let path = store.path(&workspace());
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains("sandbox"), "{text}");
    write_file(&path, &text.replace("phase/run/v5", "phase/run/v3"));

    let record = on_disk(&store);

    assert_eq!(record.processes[0].sandbox, None);
}

#[test]
fn a_launched_host_process_records_its_sandbox_in_the_record() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    journal
        .launching(&app("api"), &process("server"), &launching(500, 1))
        .unwrap();
    drop(journal);

    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(store.path(&workspace())).unwrap()).unwrap();

    assert_eq!(json["schema"], "phase/run/v5");
    assert_eq!(
        json["processes"][0]["sandbox"],
        serde_json::json!({ "kind": "seatbelt", "partial": crate::sandbox::SEATBELT_PARTIAL })
    );
}

#[test]
fn an_exit_of_zero_of_a_launched_oneshot_records_its_completion_until_it_launches_again() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = launched_oneshot(&store);
    assert_eq!(on_disk(&store).processes[0].oneshot, Some(oneshot_launch()));

    journal
        .ending(&server(), Ended::by_itself(Some(Exit::Code(0))))
        .unwrap();

    let entry = on_disk(&store).processes[0].clone();
    assert_eq!(entry.completed, Some(PlanHash::new([7; 32])));
    assert_eq!(entry.state, EntryState::Stopping);
    // A stop that phase begins after the completion keeps the end.
    journal.stopping(&server()).unwrap();
    journal.stopped(&server()).unwrap();
    assert_eq!(on_disk(&store).processes[0].ended, entry.ended);
    assert_eq!(
        on_disk(&store).processes[0].completed,
        Some(PlanHash::new([7; 32]))
    );
    assert!(journal.record().unwrap().holds_an_exit());

    journal
        .launching(&app("api"), &process("server"), &launching(300, 9))
        .unwrap();

    let entry = on_disk(&store).processes[0].clone();
    assert_eq!(entry.completed, None);
    assert_eq!(entry.oneshot, None);
}

#[test]
fn a_keeper_that_records_an_exit_of_zero_after_the_lock_is_free_completes_the_oneshot() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    drop(launched_oneshot(&store));
    let mut keeper = Journal::open(&store, &workspace(), SHORT).unwrap();

    keeper
        .stopped_after(&server(), Ended::by_itself(Some(Exit::Code(0))))
        .unwrap();

    let entry = on_disk(&store).processes[0].clone();
    assert_eq!(entry.completed, Some(PlanHash::new([7; 32])));
    assert_eq!(entry.state, EntryState::Stopped);
}

#[test]
fn no_end_but_an_exit_of_zero_by_itself_of_a_oneshot_records_a_completion() {
    for (oneshot, ended) in [
        (true, Ended::by_itself(Some(Exit::Code(1)))),
        (true, Ended::by_itself(Some(Exit::Signal(9)))),
        (true, Ended::by_itself(None)),
        (true, Ended::requested(Some(Exit::Code(0)))),
        (false, Ended::by_itself(Some(Exit::Code(0)))),
    ] {
        let state = tempfile::tempdir().unwrap();
        let store = RunStore::new(state.path());
        let mut journal = if oneshot {
            launched_oneshot(&store)
        } else {
            let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
            journal.plan(planned()).unwrap();
            journal
                .launching(&app("api"), &process("server"), &launching(300, 1))
                .unwrap();
            journal.started(&server(), Started::Host(host())).unwrap();
            journal
        };

        journal.ending(&server(), ended).unwrap();

        assert_eq!(
            on_disk(&store).processes[0].completed,
            None,
            "{oneshot} {ended:?}"
        );
    }
}

#[test]
fn clearing_a_completion_forgets_the_hash_and_the_end_and_leaves_other_entries_unwritten() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = launched_oneshot(&store);
    assert!(!journal.clear_completion(&server()).unwrap());
    journal
        .ending(&server(), Ended::by_itself(Some(Exit::Code(0))))
        .unwrap();

    assert!(journal.clear_completion(&server()).unwrap());

    let entry = on_disk(&store).processes[0].clone();
    assert_eq!(entry.completed, None);
    assert_eq!(entry.ended, None);
}

#[test]
fn a_launched_oneshot_records_its_hash_and_the_runs_it_depends_on_as_named_fields() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    drop(launched_oneshot(&store));
    let text = fs::read_to_string(store.path(&workspace())).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();

    assert_eq!(
        json["processes"][0]["oneshot"]["depends"],
        serde_json::json!([
            {
                "kind": "host",
                "app": "cache",
                "process": "server",
                "keeper": {
                    "pid": 4242,
                    "started_at_us": 1_793_000_000_000_000_u64,
                    "group": 4242,
                    "boot_id": null
                }
            },
            {
                "kind": "container",
                "app": "db",
                "process": "postgres",
                "id": "c0".repeat(32),
                "started_at": "2026-11-05T09:00:00.123456789Z"
            }
        ])
    );
    assert_eq!(
        json["processes"][0]["oneshot"]["plan_hash"],
        serde_json::json!(PlanHash::new([7; 32]))
    );
}

#[test]
fn a_v4_record_reads_as_one_whose_processes_were_not_launched_as_oneshots_or_completed() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut journal = Journal::open(&store, &workspace(), SHORT).unwrap();
    journal.plan(planned()).unwrap();
    journal
        .launching(&app("api"), &process("server"), &launching(300, 1))
        .unwrap();
    journal.started(&server(), Started::Host(host())).unwrap();
    drop(journal);
    let path = store.path(&workspace());
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains("completed"), "{text}");
    assert!(!text.contains("oneshot"), "{text}");
    write_file(&path, &text.replace("phase/run/v5", "phase/run/v4"));

    let entry = on_disk(&store).processes[0].clone();
    assert_eq!(entry.oneshot, None);
    assert_eq!(entry.completed, None);
    assert_eq!(entry.host, Some(host()));
}
