use std::time::Duration;

use super::{Waits, sweep};
use crate::record::docker::{Docker, DockerKind, Unreachable};
use crate::record::run::{
    EntryState, HostProcess, NetworkId, ObjectKey, RunRecord, Started, VolumeLifetime,
};
use crate::record::store::{Journal, RunStore};
use crate::record::testing::{FakeDocker, Spawned, app, empty_record, process, workspace};
use crate::record::{ContainerId, DockerName, Handle, ObjectKind, Role, SweepOutcome, Swept};

const WAITS: Waits = Waits {
    lock: Duration::from_millis(50),
    kill: Duration::from_secs(5),
};

fn server() -> ObjectKey {
    ObjectKey::Process(app("api"), process("server"))
}

fn name(text: &str) -> DockerName {
    text.parse().unwrap()
}

fn container_id() -> String {
    "c0".repeat(32)
}

fn network_id() -> String {
    "4e".repeat(32)
}

/// Writes a record whose `api/server` process is `host`, and whose keeper
/// never started.
fn record_host(store: &RunStore, host: HostProcess) {
    let mut journal = Journal::open(store, &workspace(), WAITS.lock).unwrap();
    journal
        .plan(empty_record().host_process(&app("api"), &process("server")))
        .unwrap();
    journal.started(&server(), Started::Host(host)).unwrap();
}

/// Writes `planned`, and then marks each of `started` as started.
fn record(store: &RunStore, planned: RunRecord, started: Vec<(ObjectKey, Started)>) {
    let mut journal = Journal::open(store, &workspace(), WAITS.lock).unwrap();
    journal.plan(planned).unwrap();
    for (key, how) in started {
        journal.started(&key, how).unwrap();
    }
}

fn run(store: &RunStore, dry_run: bool, docker: &dyn Docker) -> Swept {
    sweep(store, &workspace(), dry_run, docker, WAITS).unwrap()
}

fn outcome_of(swept: &Swept, role: Role) -> SweepOutcome {
    swept
        .items
        .iter()
        .find(|item| item.role == role)
        .unwrap()
        .outcome
}

#[test]
fn sweep_stops_a_recorded_host_process_and_removes_the_record() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut sleeper = Spawned::sleeper();
    let host = sleeper.host();
    record_host(&store, host);

    let swept = run(&store, false, &Unreachable);

    assert!(!sleeper.running());
    assert_eq!(outcome_of(&swept, Role::Process), SweepOutcome::Stopped);
    assert_eq!(outcome_of(&swept, Role::Keeper), SweepOutcome::Gone);
    let item = swept
        .items
        .iter()
        .find(|item| item.role == Role::Process)
        .unwrap();
    assert_eq!(item.handle, Some(Handle::pid(host.pid, host.started_at_us)));
    assert_eq!(store.read(&workspace()).unwrap(), None);
}

#[test]
fn sweep_leaves_a_process_whose_start_time_differs_and_reports_the_pid_as_reused() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut sleeper = Spawned::sleeper();
    let real = sleeper.host();
    let fake = HostProcess {
        started_at_us: real.started_at_us - 1,
        ..real
    };
    record_host(&store, fake);

    let swept = run(&store, false, &Unreachable);

    assert!(sleeper.running());
    assert_eq!(outcome_of(&swept, Role::Process), SweepOutcome::PidReused);
    assert_eq!(store.read(&workspace()).unwrap(), None);
}

#[test]
fn sweep_kills_a_process_that_ignores_term_once_the_kill_wait_passes() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut stubborn = Spawned::shell("trap '' TERM; echo ready; while :; do sleep 1; done");
    record_host(&store, stubborn.host());
    let waits = Waits {
        kill: Duration::from_millis(200),
        ..WAITS
    };

    let swept = sweep(&store, &workspace(), false, &Unreachable, waits).unwrap();

    assert!(!stubborn.running());
    assert_eq!(outcome_of(&swept, Role::Process), SweepOutcome::Stopped);
}

#[test]
fn sweep_reports_a_process_that_has_already_ended_as_gone() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let ended = {
        let sleeper = Spawned::sleeper();
        sleeper.host()
    };
    record_host(&store, ended);

    let swept = run(&store, false, &Unreachable);

    assert_eq!(outcome_of(&swept, Role::Process), SweepOutcome::Gone);
    assert_eq!(store.read(&workspace()).unwrap(), None);
}

#[test]
fn a_dry_run_reports_what_sweep_would_stop_and_changes_nothing() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut sleeper = Spawned::sleeper();
    record_host(&store, sleeper.host());
    let before = store.read(&workspace()).unwrap();

    let swept = run(&store, true, &Unreachable);

    assert!(sleeper.running());
    assert_eq!(outcome_of(&swept, Role::Process), SweepOutcome::WouldStop);
    assert_eq!(store.read(&workspace()).unwrap(), before);
}

#[test]
fn a_dry_run_works_while_another_operation_holds_the_lock() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let sleeper = Spawned::sleeper();
    record_host(&store, sleeper.host());
    let _held = store.lock(&workspace(), WAITS.lock).unwrap();

    let swept = run(&store, true, &Unreachable);

    assert_eq!(outcome_of(&swept, Role::Process), SweepOutcome::WouldStop);
}

#[test]
fn sweep_fails_as_busy_while_another_operation_holds_the_lock() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let mut sleeper = Spawned::sleeper();
    record_host(&store, sleeper.host());
    let _held = store.lock(&workspace(), WAITS.lock).unwrap();

    let error = sweep(&store, &workspace(), false, &Unreachable, WAITS).unwrap_err();

    assert_eq!(error.code(), "phase::lock::busy");
    assert!(sleeper.running());
}

fn docker_run() -> RunRecord {
    empty_record()
        .container(
            &app("db"),
            &process("server"),
            name("mxv-0b9f3c8e-db-server-1"),
        )
        .network(name("mxv-0b9f3c8e"))
        .volume(name("mxv-0b9f3c8e-cache"), VolumeLifetime::Run)
        .volume(name("mxv-0b9f3c8e-data"), VolumeLifetime::Workspace)
}

fn started_docker_objects() -> Vec<(ObjectKey, Started)> {
    vec![
        (
            ObjectKey::Container(app("db"), process("server")),
            Started::Container(ContainerId::try_from(container_id()).unwrap()),
        ),
        (
            ObjectKey::Network(name("mxv-0b9f3c8e")),
            Started::Network(NetworkId::try_from(network_id()).unwrap()),
        ),
        (
            ObjectKey::Volume(name("mxv-0b9f3c8e-cache")),
            Started::Volume,
        ),
        (
            ObjectKey::Volume(name("mxv-0b9f3c8e-data")),
            Started::Volume,
        ),
    ]
}

fn running_docker() -> FakeDocker {
    FakeDocker::default()
        .with(
            DockerKind::Container,
            "mxv-0b9f3c8e-db-server-1",
            &container_id(),
        )
        .with(DockerKind::Network, "mxv-0b9f3c8e", &network_id())
        .with(
            DockerKind::Volume,
            "mxv-0b9f3c8e-cache",
            "mxv-0b9f3c8e-cache",
        )
        .with(DockerKind::Volume, "mxv-0b9f3c8e-data", "mxv-0b9f3c8e-data")
}

#[test]
fn sweep_removes_the_recorded_container_network_and_run_volume_and_keeps_the_data_volume() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    record(&store, docker_run(), started_docker_objects());
    let docker = running_docker();

    let swept = run(&store, false, &docker);

    assert_eq!(
        *docker.removed.borrow(),
        vec![
            container_id(),
            network_id(),
            "mxv-0b9f3c8e-cache".to_owned()
        ]
    );
    let left: Vec<String> = docker
        .objects
        .borrow()
        .iter()
        .map(|object| object.name.clone())
        .collect();
    assert_eq!(left, vec!["mxv-0b9f3c8e-data".to_owned()]);
    assert_eq!(swept.items[0].outcome, SweepOutcome::Stopped);
    let objects: Vec<(ObjectKind, SweepOutcome)> = swept
        .objects
        .iter()
        .map(|object| (object.kind, object.outcome))
        .collect();
    assert_eq!(
        objects,
        vec![
            (ObjectKind::Network, SweepOutcome::Stopped),
            (ObjectKind::Volume, SweepOutcome::Stopped),
            (ObjectKind::Volume, SweepOutcome::Kept),
        ]
    );
    assert_eq!(store.read(&workspace()).unwrap(), None);
}

#[test]
fn sweep_finds_a_container_that_started_before_its_id_was_recorded_by_its_name() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let planned = empty_record().container(
        &app("db"),
        &process("server"),
        name("mxv-0b9f3c8e-db-server-1"),
    );
    record(&store, planned, Vec::new());
    let docker = FakeDocker::default().with(
        DockerKind::Container,
        "mxv-0b9f3c8e-db-server-1",
        &container_id(),
    );

    let swept = run(&store, false, &docker);

    assert_eq!(swept.items[0].outcome, SweepOutcome::Stopped);
    let id = ContainerId::try_from(container_id()).unwrap();
    assert_eq!(swept.items[0].handle, Some(Handle::container(id)));
    assert!(docker.objects.borrow().is_empty());
}

#[test]
fn sweep_fails_when_docker_does_not_answer_and_keeps_the_record() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    record(&store, docker_run(), started_docker_objects());
    let docker = FakeDocker {
        down: true,
        ..running_docker()
    };

    let error = sweep(&store, &workspace(), false, &docker, WAITS).unwrap_err();

    assert_eq!(error.code(), "phase::docker::unavailable");
    let kept = store.read(&workspace()).unwrap().unwrap();
    assert_eq!(kept.containers[0].state, EntryState::Running);
    assert!(sweep(&store, &workspace(), true, &docker, WAITS).is_err());
}

#[test]
fn a_container_that_docker_fails_to_remove_is_unstoppable_and_stays_recorded() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    let planned = empty_record().container(
        &app("db"),
        &process("server"),
        name("mxv-0b9f3c8e-db-server-1"),
    );
    record(&store, planned, Vec::new());
    let docker = FakeDocker {
        stubborn: true,
        ..FakeDocker::default().with(
            DockerKind::Container,
            "mxv-0b9f3c8e-db-server-1",
            &container_id(),
        )
    };

    let swept = run(&store, false, &docker);

    assert_eq!(swept.items[0].outcome, SweepOutcome::Unstoppable);
    let kept = store.read(&workspace()).unwrap().unwrap();
    assert_eq!(kept.containers[0].state, EntryState::Stopping);
}

#[test]
fn a_dry_run_reports_the_docker_objects_it_would_remove_and_removes_none() {
    let state = tempfile::tempdir().unwrap();
    let store = RunStore::new(state.path());
    record(&store, docker_run(), started_docker_objects());
    let docker = running_docker();

    let swept = run(&store, true, &docker);

    assert!(docker.removed.borrow().is_empty());
    assert_eq!(swept.items[0].outcome, SweepOutcome::WouldStop);
    assert!(store.read(&workspace()).unwrap().is_some());
}
